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

pub mod client;

use modron_types::{
    AgentState, Attachment, Effort, ModelRoles, Phase, PlanItem, RunSummary, SandboxPolicy,
    SessionEvent, StreamDelta, ToolActivity, ToolCall, Usage,
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

/// Why a server-side failure happened (clients branch on this without
/// string matching: auto re-auth on Auth, retry on Busy, ...).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// Credentials missing/expired/revoked - re-auth fixes it.
    Auth,
    /// Unknown id / missing resource.
    NotFound,
    /// The request conflicts with live state (busy session, read-only).
    Conflict,
    /// The connection is not allowed to perform this request, e.g. a
    /// share-link tunnel asking for a server-wide action; re-auth does not
    /// help.
    Forbidden,
    /// Everything else (config errors, transport, internal).
    Other,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
// A Snapshot carries the whole live state by value. Payloads are
// short-lived (built, serialized, dropped) and boxing one variant would
// change every construction site in four clients for no real saving.
#[allow(clippy::large_enum_variant)]
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
    /// Steer one live sub-agent of a RUNNING session (PROTOCOL_V2 section 4,
    /// H3d): `text` joins that child's own transcript at its next safe
    /// boundary, so its next model turn carries it. `agent` is the swarm
    /// row's `id`. Errors (requester only) when no such live child exists.
    /// No broadcast on success: the swarm row is unchanged, the child's
    /// transcript shows the message. UI effect: a composer action on a row.
    SteerAgent {
        session_id: Uuid,
        agent: String,
        text: String,
    },
    /// Stop one live sub-agent (PROTOCOL_V2 section 4, H3d). Only that child
    /// ends: its parent run and siblings continue. The row turns `killed` in
    /// place (a `tool` frame), and the parent's `delegate` call (or
    /// `delegate_status`) reports the stop. Errors (requester only) when no
    /// such live child exists. A kill while an approval ask of that child is
    /// pending takes effect when the ask is answered or times out.
    KillAgent { session_id: Uuid, agent: String },
    /// Send a message to a RUNNING session: it is injected at the next safe
    /// boundary (between model turns, after pending tool results) and the
    /// run continues. An idle session refuses it (submit instead). Replies
    /// `Steered` on success; the injected text persists as a `Task` event.
    Steer { session_id: Uuid, text: String },
    /// Hold a message for the session's NEXT run. While a run is active the
    /// server keeps it in the session queue and starts it as a new run when
    /// the current run finishes successfully (never after a failure or an
    /// interrupt). Replies `QueueChanged` to the session's subscribers. An
    /// idle session starts the run at once, exactly like `Submit`.
    Enqueue { session_id: Uuid, text: String },
    /// Remove one queued message by its position (0 = next to run). Replies
    /// `QueueChanged`; an out-of-range index is an error.
    Dequeue { session_id: Uuid, index: u32 },
    /// Compact an IDLE session now (H9b, `/compact`): the older transcript is
    /// summarized by the same routine autocompaction uses, ignoring the
    /// threshold and the min-interval rule. Refused while a run is active
    /// (error "session busy - compact when idle"). Replies with a correlated
    /// `Note` (`compacted: N -> M tokens`, or the nothing-to-compact note),
    /// then `ContextChanged` with the refreshed gauge.
    Compact { session_id: Uuid },
    /// Arm a scheduled wake for the session (H5a). One wake per session: a
    /// new one replaces the old. Exactly one of `after_minutes` (one-shot,
    /// 1..=10080) or `every_minutes` (recurring, 5..=10080) must be set, and
    /// `prompt` must not be empty. When the wake fires on an idle session the
    /// prompt starts a run through the same path a queued follow-up uses; on
    /// a busy session it is queued. Replies by publishing `WakeChanged` to
    /// subscribers; bad arguments are an error.
    Wake {
        session_id: Uuid,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after_minutes: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        every_minutes: Option<u32>,
        prompt: String,
    },
    /// Cancel the session's armed wake (publishes `WakeChanged { wake: None }`).
    /// Errors when no wake is armed.
    CancelWake { session_id: Uuid },
    /// Answer a pending approval (first responder wins).
    Approve {
        request_id: String,
        decision: Decision,
    },
    /// Answer a proposed plan (H2b). `Approve` flips the session to `build`
    /// and starts a run that executes the plan (`note`, when present, is
    /// appended). `Revise` keeps `plan` and starts another plan run with the
    /// `note` as the task (required). Errors when no plan is pending. Replies
    /// with `PlanDecided` to subscribers, then the run's own frames.
    DecidePlan {
        session_id: Uuid,
        decision: PlanDecision,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    /// Mutate session/engine config for future turns.
    SetConfig { patch: ConfigPatch },
    /// Ask which permission rules decide a hypothetical tool call in this
    /// session (the rules as the session holds them now). `args` are the
    /// call's JSON arguments, e.g. `{"command": "rm -rf x"}`. Answered by a
    /// correlated `note` whose text is one line per rule (`matched` or `no
    /// match (reason)`) and a final `result: allow|ask|deny|no rule` line.
    /// UI effect: shows the note (the explain view / scrollback note); it
    /// changes nothing. Scoped like `steer`.
    ExplainRule {
        session_id: Uuid,
        tool: String,
        #[serde(default)]
        args: serde_json::Value,
    },
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
    /// Ask for provider auth status rows (answered by `auth_providers`).
    /// (The reply-shaped `AuthProviders { rows: [] }` still works for
    /// older clients; new clients send this.)
    AuthProvidersReq {},

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
    /// `agent` names the asking sub-agent (H3b) and is absent for the run's
    /// own calls. UI effect: append to the approval FIFO; the front shows
    /// the ask, prefixed `[agent] ` when `agent` is present.
    ApprovalRequested {
        request_id: String,
        call: ToolCall,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
    },
    /// The decision landed (echoed to every client).
    ApprovalResolved {
        request_id: String,
        decision: Decision,
    },
    /// A steer was accepted and queued for injection into the active run.
    /// UI effect: show `text` as a pending (dim) task echo until its
    /// persisted `Event { event: SessionEvent::Task }` arrives with the same
    /// text, which replaces the pending echo in transcript order.
    Steered { text: String },
    /// The session's queue changed (enqueue, dequeue, or a queued message
    /// starting its run). `queued` is the full list, front first.
    /// UI effect: REPLACE the queue chips above the composer with `queued`
    /// (an empty list clears them). Snapshot's `LiveState.queued` resets
    /// them the same way on attach.
    QueueChanged { queued: Vec<String> },
    /// The session's context occupancy was recomputed: after every run ends
    /// (success, failure or abort) and after a `Compact`. `used` is the
    /// estimated size of the next request in tokens; `window` is the
    /// effective per-turn input window it is measured against.
    /// UI effect: SET the status surface's context meter to `used / window`.
    /// This is the context figure; cumulative tokens are not context.
    /// Snapshot's `LiveState.context` shows the last value on attach.
    ContextChanged { used: u64, window: u64 },
    /// The session's armed wake changed: set, replaced, cancelled, fired (a
    /// one-shot clears, a recurring one re-arms with a new `next_at_unix`).
    /// UI effect: SET the status surface's wake line from `wake` ("wakes in
    /// 12m", formatted client-side from `next_at_unix`); `None` clears it.
    /// Snapshot's `LiveState.wake` shows it on attach.
    WakeChanged { wake: Option<WakeWire> },
    /// An event worth a push for devices that are not attached (H5a). Never
    /// rendered in the transcript. UI effect: an attached client MAY show
    /// `text` as a toast; the relay turns the frame into a push for detached
    /// devices. Sent for run done / failed, proposed plans and fired wakes;
    /// never for an interrupt. Approval requests are not notified yet (the
    /// `approval` kind is reserved).
    Notify { kind: NotifyKind, text: String },
    /// A BACKGROUND sub-agent settled (PROTOCOL_V2 section 4, H3d): `agent`
    /// is its swarm-row id, `ok` is false for a failure or a user kill, and
    /// `brief` is the first 200 chars of its result. It comes with a
    /// `Notify` (`done` or `failed`, text `sub-agent <label> finished` or
    /// `... failed`). UI effect: the matching swarm row already shows its
    /// final state; an attached client raises a toast when that agent is not
    /// the focused view, and the last-note line takes the notify text.
    /// Foreground sub-agents never send it. `delegate_status` polling stays
    /// for the model.
    AgentFinished {
        agent: String,
        ok: bool,
        brief: String,
    },
    /// Config mutated (echo of the applied patch).
    /// UI effect: a `phase` in the patch updates the phase label on the
    /// status surface (`plan` / `build`); a `rules` list replaces the
    /// rules the settings view lists; the other fields as before.
    ConfigChanged { patch: ConfigPatch },
    /// A plan-phase run finished with a plan: the run is paused and waits
    /// for a decision. `items` is the proposed plan.
    /// UI effect: BLOCKS on a decision exactly like an approval (the strip,
    /// buttons or sheet per target), showing `items`. Queued follow-ups
    /// wait until the decision. Snapshot's `LiveState.plan_pending` shows it
    /// on attach.
    PlanProposed { items: Vec<PlanItem> },
    /// The plan decision landed (echoed to every client).
    /// UI effect: CLEARS the plan decision block opened by `PlanProposed`.
    PlanDecided { decision: PlanDecision },
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
    /// Start a provider's sign-in flow on the SERVER (the browser opens
    /// there); progress streams to `session_id` as `oauth_progress`
    /// frames and completion lands as a `note`. OAuth-capable providers
    /// only (anthropic, openai-compat, zai).
    AuthStart { session_id: Uuid, provider: String },
    /// Save an API key for a provider (the non-oauth path); answers with
    /// a `note`.
    AuthSetKey {
        session_id: Uuid,
        provider: String,
        key: String,
    },
    /// The router's default-model picture: what is signed in, the
    /// derived recommendation, and the active fallback chains. The
    /// settings page prefills its defaults card from this.
    RouterDefaultsReq {},
    /// The reply to `router_defaults_req` (settings page, defaults card).
    RouterDefaults {
        authed: Vec<String>,
        recommended_cheap: Option<String>,
        recommended_strong: Option<String>,
        cheap_fallback: Vec<String>,
        strong_fallback: Vec<String>,
    },
    /// An informational toast/scrollback note (slash-command results).
    Note { text: String },
    /// Answer to `ping`.
    Pong {},
    /// A request failed (echoes the request `id`). `kind` lets clients
    /// react without string matching (e.g. auto re-auth on auth errors);
    /// absent on older servers.
    Error {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<ErrorKind>,
    },
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

/// The answer to a proposed plan (`DecidePlan` / `PlanDecided`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanDecision {
    /// Flip to `build` and execute the plan.
    Approve,
    /// Stay in `plan` and plan again with the note.
    Revise,
}

impl Decision {
    /// Every decision, in protocol order (approval sheet and strip rows).
    pub const ALL: [Decision; 3] = [Decision::AllowOnce, Decision::AllowAll, Decision::Deny];

    /// The shared approval-decision words (AGENTS.md invariant 1).
    pub const fn label(self) -> &'static str {
        match self {
            Decision::AllowOnce => "allow once",
            Decision::AllowAll => "allow all",
            Decision::Deny => "deny",
        }
    }
}

/// One permission rule on the wire (`[permissions]` in config.toml, the
/// same grammar). `effect` is `"allow"`, `"ask"` or `"deny"`; `spec` is
/// `tool` or `tool(pattern)`, e.g. `bash(cargo test*)` or `read`.
///
/// UI effect: the settings view lists each rule as `effect spec`. The
/// server rejects a whole `ConfigPatch` when any entry does not parse.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleWire {
    pub effect: String,
    pub spec: String,
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
    /// The run phase for future runs: `plan` (read-only) or `build`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<Phase>,
    /// The whole permission rule set for future runs (`allow`/`ask`/`deny`
    /// rules). When present it REPLACES the session's rules; an empty list
    /// clears them. Any unparsable entry rejects the whole patch with an
    /// error that names that rule, and nothing changes. Echoed back in
    /// `ConfigChanged`; `LiveState.rules` reports the result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules: Option<Vec<RuleWire>>,
    /// The reasoning effort for future turns (`low` .. `max`). Absent leaves
    /// it alone. UI effect (ConfigChanged, every client): shows
    /// `effort <label>` next to the model on the status surface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    /// Clears the session's effort back to the provider default (no label).
    /// The "unset" form, because `effort` alone cannot say "none". Wins over
    /// `effort` when both are sent. Absent or false leaves it alone.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clear_effort: bool,
    /// The named model roles for future turns (H7c: `plan`, `subagent`,
    /// `critic`, `summary`). Present REPLACES the whole set; an all-unset
    /// value clears it. Absent leaves the roles alone. Echoed in
    /// `ConfigChanged`; `LiveState.roles` reports the result. UI effect
    /// (ConfigChanged, every client): the settings list shows the four roles
    /// and their model ids, with unset roles shown as `router`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roles: Option<Box<ModelRoles>>,
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

impl ApprovalModeWire {
    /// The approval-mode word shown in every client's status surface.
    pub const fn label(self) -> &'static str {
        match self {
            ApprovalModeWire::Deny => "deny",
            ApprovalModeWire::Ask => "ask",
            ApprovalModeWire::ApproveAll => "yolo",
        }
    }
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

/// The router defaults picture for the settings page (reply to
/// `router_defaults_req`): authed catalog providers, the derived
/// recommendation, and the live fallback chains.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouterDefaults {
    pub authed: Vec<String>,
    pub recommended_cheap: Option<String>,
    pub recommended_strong: Option<String>,
    pub cheap_fallback: Vec<String>,
    pub strong_fallback: Vec<String>,
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

/// The session's context occupancy on the wire (H9b): the estimated size of
/// the next request (`used`) and the effective window it is measured
/// against (`window`), both in tokens. Mirror of core `ContextGauge`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextGaugeWire {
    pub used: u64,
    pub window: u64,
}

/// The live (non-persisted) half of a snapshot: what a fresh attach needs
/// to render the in-flight state a late joiner would otherwise miss.
///
/// UI effects on attach (Snapshot handling, every client):
/// - `approval_mode`: sets the approval-mode label (`ask`/`yolo`/`deny`)
///   through the same mapping ConfigChanged uses. Absent keeps the
///   client's default label (`ask`).
/// - `sandbox`: sets the sandbox label (`off`/`seatbelt`/`bwrap`) through
///   the same mapping ConfigChanged uses. Absent keeps the default (`off`).
/// - `allow` / `deny`: replace the client's copy of the server's tool
///   allow and deny lists (the TUI's `RemoteLists`, the mobile tool
///   editor). Absent means the server has no entries, so the copy is
///   emptied; a SetConfig sends the whole list back.
/// - `rules`: replace the client's copy of the permission rules (the
///   settings view lists them as `effect spec`). Empty or absent means
///   the session has no rules; a SetConfig `rules` patch sends the whole
///   set back.
///
/// The four config fields are optional on the wire (`skip_serializing_if`
/// and serde defaults), so an older peer that omits them still parses,
/// and a newer server talking to an older client is ignored by it.
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
    /// Messages held for the next run, front first (see `Enqueue`). UI
    /// effect: the queue chips above the composer; `QueueChanged` replaces
    /// them. Omitted on the wire when empty (older peers never send it).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued: Vec<String>,
    /// The session's context occupancy (see `ContextChanged`). UI effect:
    /// sets the context meter on the status surface (`used / window`);
    /// absent until the session has run or been compacted on this server,
    /// and the client keeps its meter hidden.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<ContextGaugeWire>,
    /// The server's approval mode (see the struct docs: sets the label).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_mode: Option<ApprovalModeWire>,
    /// The server's sandbox policy (see the struct docs: sets the label).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<SandboxPolicy>,
    /// The server's tool allow list (see the struct docs).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    /// The server's tool deny list (see the struct docs).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
    /// The session's permission rules, in evaluation order (see
    /// `RuleWire`). UI effect: replaces the rules listed in settings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<RuleWire>,
    /// The session's run phase. UI effect: sets the phase label on the
    /// status surface through the same mapping ConfigChanged uses. Absent
    /// keeps the client's default (`build`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<Phase>,
    /// The reasoning effort for future turns. UI effect: shows
    /// `effort <label>` next to the model on the status surface through the
    /// same mapping ConfigChanged uses. Absent means the provider default
    /// and no label. Older peers omit it and still parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    /// The named model roles in force (H7c). UI effect: the settings list
    /// shows each role and its model id, unset roles as `router`. Omitted
    /// when every role is unset; older peers omit it and still parse.
    #[serde(default, skip_serializing_if = "ModelRoles::is_empty")]
    pub roles: ModelRoles,
    /// A proposed plan awaits a decision (see `PlanProposed`). UI effect:
    /// shows the plan decision block on attach. Omitted when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub plan_pending: bool,
    /// The session's armed wake (see `WakeChanged`). UI effect: the wake
    /// line on the status surface. Absent when no wake is armed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake: Option<WakeWire>,
}

/// A scheduled wake on the wire (H5a): when it next fires and what it runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WakeWire {
    /// Epoch seconds of the next firing.
    pub next_at_unix: u64,
    /// Recurrence in minutes; absent for a one-shot wake.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every_minutes: Option<u32>,
    /// The task the wake runs when it fires.
    pub prompt: String,
}

/// Why a `Notify` frame was sent (snake_case on the wire).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotifyKind {
    /// An approval is waiting for a decision. Reserved: not sent yet.
    Approval,
    /// A run completed.
    Done,
    /// A run failed.
    Failed,
    /// A plan is ready for review.
    Plan,
    /// A scheduled wake fired.
    Wake,
}

/// An approval awaiting a decision.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingApproval {
    pub request_id: String,
    pub call: ToolCall,
    /// The asking sub-agent's label, when a child asked (H3b).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
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
        /// The model's tool-call id. Absent on frames from older peers and
        /// omitted when `None`, so pre-LM JSON parses and round-trips.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
    },
    Done {
        name: String,
        is_error: bool,
        bytes: u64,
        /// Matches the `call_id` of the `Start` it completes (see `Start`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
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
    /// Mirror of core `ToolActivity::SubagentState` (H3d): the row's state
    /// word updates in place.
    SubagentState {
        id: String,
        label: String,
        state: AgentState,
    },
    /// Mirror of core `ToolActivity::BackgroundFinished` (H3d).
    BackgroundFinished {
        id: String,
        label: String,
        ok: bool,
        brief: String,
    },
}

impl From<&ToolActivity> for WireToolActivity {
    fn from(a: &ToolActivity) -> Self {
        match a {
            ToolActivity::Start { name, call_id } => Self::Start {
                name: name.clone(),
                call_id: call_id.clone(),
            },
            ToolActivity::Done {
                name,
                is_error,
                bytes,
                call_id,
            } => Self::Done {
                name: name.clone(),
                is_error: *is_error,
                bytes: *bytes as u64,
                call_id: call_id.clone(),
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
            ToolActivity::SubagentState { id, label, state } => Self::SubagentState {
                id: id.clone(),
                label: label.clone(),
                state: *state,
            },
            ToolActivity::BackgroundFinished {
                id,
                label,
                ok,
                brief,
            } => Self::BackgroundFinished {
                id: id.clone(),
                label: label.clone(),
                ok: *ok,
                brief: brief.clone(),
            },
        }
    }
}

impl From<WireToolActivity> for ToolActivity {
    fn from(w: WireToolActivity) -> Self {
        match w {
            WireToolActivity::Start { name, call_id } => Self::Start { name, call_id },
            WireToolActivity::Done {
                name,
                is_error,
                bytes,
                call_id,
            } => Self::Done {
                name,
                is_error,
                bytes: bytes as usize,
                call_id,
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
            WireToolActivity::SubagentState { id, label, state } => {
                Self::SubagentState { id, label, state }
            }
            WireToolActivity::BackgroundFinished {
                id,
                label,
                ok,
                brief,
            } => Self::BackgroundFinished {
                id,
                label,
                ok,
                brief,
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
    /// Lifecycle state (H3d): `running`, `waiting_approval`, `done`,
    /// `failed` or `killed`. Absent from older servers, where it reads as
    /// running (the `done` flag still decides done and failed).
    #[serde(default)]
    pub state: AgentState,
}

#[cfg(test)]
mod tests {
    use super::*;
    use modron_types::Message;

    fn roundtrip(frame: &Frame) -> Frame {
        let json = serde_json::to_string(frame).expect("serialize");
        serde_json::from_str(&json).expect("deserialize")
    }

    #[test]
    fn shared_label_words_are_stable() {
        assert_eq!(Decision::AllowOnce.label(), "allow once");
        assert_eq!(Decision::AllowAll.label(), "allow all");
        assert_eq!(Decision::Deny.label(), "deny");
        assert_eq!(ApprovalModeWire::Deny.label(), "deny");
        assert_eq!(ApprovalModeWire::Ask.label(), "ask");
        assert_eq!(ApprovalModeWire::ApproveAll.label(), "yolo");
    }

    /// `forbidden` is a wire name clients can branch on: it serializes as
    /// the bare string and round-trips inside an error frame.
    #[test]
    fn forbidden_error_kind_roundtrips_as_forbidden() {
        assert_eq!(
            serde_json::to_string(&ErrorKind::Forbidden).unwrap(),
            "\"forbidden\""
        );
        let frame = Frame {
            id: Some(9),
            session: None,
            seq: 0,
            payload: Payload::Error {
                message: "not allowed on a share link".into(),
                kind: Some(ErrorKind::Forbidden),
            },
        };
        let json = serde_json::to_value(&frame).unwrap();
        assert_eq!(json["kind"], "forbidden");
        let back: Frame = serde_json::from_value(json).unwrap();
        match back.payload {
            Payload::Error { kind, .. } => assert_eq!(kind, Some(ErrorKind::Forbidden)),
            other => panic!("expected an error frame, got {other:?}"),
        }
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

    /// Steer (client) and Steered (server) carry their text and round-trip
    /// with the snake_case tags the other clients use.
    #[test]
    fn steer_frames_roundtrip_with_tags() {
        let session = Uuid::new_v4();
        let steer = Frame::request(
            9,
            Payload::Steer {
                session_id: session,
                text: "use the other crate".into(),
            },
        );
        assert_roundtrip(&steer);
        let json = serde_json::to_value(&steer).unwrap();
        assert_eq!(json["type"], "steer");
        assert_eq!(json["text"], "use the other crate");

        let steered = Frame::event(
            Some(session),
            10,
            Payload::Steered {
                text: "use the other crate".into(),
            },
        );
        assert_roundtrip(&steered);
        let json = serde_json::to_value(&steered).unwrap();
        assert_eq!(json["type"], "steered");
        assert_eq!(json["text"], "use the other crate");
    }

    /// Enqueue / Dequeue (client) and QueueChanged (server) round-trip with
    /// snake_case tags, and an older LiveState without `queued` still loads.
    #[test]
    fn queue_frames_roundtrip_with_tags() {
        let session = Uuid::new_v4();
        let enqueue = Frame::request(
            11,
            Payload::Enqueue {
                session_id: session,
                text: "then run the tests".into(),
            },
        );
        assert_roundtrip(&enqueue);
        let json = serde_json::to_value(&enqueue).unwrap();
        assert_eq!(json["type"], "enqueue");
        assert_eq!(json["text"], "then run the tests");

        let dequeue = Frame::request(
            12,
            Payload::Dequeue {
                session_id: session,
                index: 1,
            },
        );
        assert_roundtrip(&dequeue);
        let json = serde_json::to_value(&dequeue).unwrap();
        assert_eq!(json["type"], "dequeue");
        assert_eq!(json["index"], 1);

        let changed = Frame::event(
            Some(session),
            13,
            Payload::QueueChanged {
                queued: vec!["a".into(), "b".into()],
            },
        );
        assert_roundtrip(&changed);
        let json = serde_json::to_value(&changed).unwrap();
        assert_eq!(json["type"], "queue_changed");
        assert_eq!(json["queued"], serde_json::json!(["a", "b"]));
    }

    /// Compact (client) and ContextChanged (server) round-trip with snake_case
    /// tags and carry their fields by name.
    #[test]
    fn compact_frames_roundtrip_with_tags() {
        let session = Uuid::new_v4();
        let compact = Frame::request(
            14,
            Payload::Compact {
                session_id: session,
            },
        );
        assert_roundtrip(&compact);
        let json = serde_json::to_value(&compact).unwrap();
        assert_eq!(json["type"], "compact");
        assert_eq!(json["session_id"], session.to_string());

        let changed = Frame::event(
            Some(session),
            15,
            Payload::ContextChanged {
                used: 41_000,
                window: 128_000,
            },
        );
        assert_roundtrip(&changed);
        let json = serde_json::to_value(&changed).unwrap();
        assert_eq!(json["type"], "context_changed");
        assert_eq!(json["used"], 41_000);
        assert_eq!(json["window"], 128_000);
    }

    /// Wake / CancelWake (client) and WakeChanged / Notify (server) round-trip
    /// with snake_case tags; the optional arguments stay off the wire when
    /// unset, and NotifyKind uses snake_case names.
    #[test]
    fn wake_and_notify_frames_roundtrip_with_tags() {
        let session = Uuid::new_v4();
        let after = Frame::request(
            20,
            Payload::Wake {
                session_id: session,
                after_minutes: Some(15),
                every_minutes: None,
                prompt: "check the build".into(),
            },
        );
        assert_roundtrip(&after);
        let json = serde_json::to_value(&after).unwrap();
        assert_eq!(json["type"], "wake");
        assert_eq!(json["after_minutes"], 15);
        assert!(json.get("every_minutes").is_none());

        let every = Frame::request(
            21,
            Payload::Wake {
                session_id: session,
                after_minutes: None,
                every_minutes: Some(30),
                prompt: "poll".into(),
            },
        );
        assert_roundtrip(&every);
        let json = serde_json::to_value(&every).unwrap();
        assert_eq!(json["every_minutes"], 30);
        assert!(json.get("after_minutes").is_none());

        let cancel = Frame::request(
            22,
            Payload::CancelWake {
                session_id: session,
            },
        );
        assert_roundtrip(&cancel);
        assert_eq!(
            serde_json::to_value(&cancel).unwrap()["type"],
            "cancel_wake"
        );

        let changed = Frame::event(
            Some(session),
            23,
            Payload::WakeChanged {
                wake: Some(WakeWire {
                    next_at_unix: 1_800_000_000,
                    every_minutes: Some(30),
                    prompt: "poll".into(),
                }),
            },
        );
        assert_roundtrip(&changed);
        let json = serde_json::to_value(&changed).unwrap();
        assert_eq!(json["type"], "wake_changed");
        assert_eq!(json["wake"]["next_at_unix"], 1_800_000_000u64);
        assert_eq!(json["wake"]["every_minutes"], 30);

        let cleared = Frame::event(Some(session), 24, Payload::WakeChanged { wake: None });
        assert_roundtrip(&cleared);
        let json = serde_json::to_value(&cleared).unwrap();
        assert_eq!(json["type"], "wake_changed");
        assert!(json["wake"].is_null());

        for (kind, tag) in [
            (NotifyKind::Approval, "approval"),
            (NotifyKind::Done, "done"),
            (NotifyKind::Failed, "failed"),
            (NotifyKind::Plan, "plan"),
            (NotifyKind::Wake, "wake"),
        ] {
            let frame = Frame::event(
                Some(session),
                25,
                Payload::Notify {
                    kind,
                    text: "done: abc".into(),
                },
            );
            assert_roundtrip(&frame);
            let json = serde_json::to_value(&frame).unwrap();
            assert_eq!(json["type"], "notify");
            assert_eq!(json["kind"], tag);
            assert_eq!(json["text"], "done: abc");
        }
    }

    /// A WakeWire omits `every_minutes` for a one-shot, and LiveState.wake is
    /// absent when no wake is armed; an older LiveState without `wake` loads.
    #[test]
    fn live_state_wake_is_optional_on_the_wire() {
        let empty = serde_json::to_value(LiveState::default()).unwrap();
        assert!(empty.get("wake").is_none());

        let filled = LiveState {
            wake: Some(WakeWire {
                next_at_unix: 42,
                every_minutes: None,
                prompt: "later".into(),
            }),
            ..LiveState::default()
        };
        let json = serde_json::to_value(&filled).unwrap();
        assert_eq!(json["wake"]["next_at_unix"], 42);
        assert!(json["wake"].get("every_minutes").is_none());
        let back: LiveState = serde_json::from_value(json).unwrap();
        assert_eq!(back.wake, filled.wake);

        let old: LiveState = serde_json::from_value(serde_json::json!({"running": false})).unwrap();
        assert!(old.wake.is_none());
    }

    /// Wake frames that omit the optional arguments still parse (an older
    /// client), and a Notify without a known kind is rejected.
    #[test]
    fn wake_frame_arguments_default_to_none() {
        let session = Uuid::new_v4();
        let json = serde_json::json!({
            "type": "wake",
            "session_id": session,
            "prompt": "ping"
        });
        let back: Payload = serde_json::from_value(json).unwrap();
        match back {
            Payload::Wake {
                after_minutes,
                every_minutes,
                prompt,
                ..
            } => {
                assert!(after_minutes.is_none() && every_minutes.is_none());
                assert_eq!(prompt, "ping");
            }
            other => panic!("expected wake, got {other:?}"),
        }
        let bad = serde_json::json!({"type": "notify", "kind": "nope", "text": "x"});
        assert!(serde_json::from_value::<Payload>(bad).is_err());
    }

    #[test]
    fn live_state_context_is_omitted_when_none_and_optional_on_read() {
        let empty = serde_json::to_value(LiveState::default()).unwrap();
        assert!(empty.get("context").is_none());

        let filled = LiveState {
            context: Some(ContextGaugeWire {
                used: 6_000,
                window: 200_000,
            }),
            ..LiveState::default()
        };
        let json = serde_json::to_value(&filled).unwrap();
        assert_eq!(json["context"]["used"], 6_000);
        assert_eq!(json["context"]["window"], 200_000);
        let back: LiveState = serde_json::from_value(json).unwrap();
        assert_eq!(
            back.context,
            Some(ContextGaugeWire {
                used: 6_000,
                window: 200_000,
            })
        );

        // A LiveState from an older peer (no `context` key) still loads.
        let old: LiveState =
            serde_json::from_str(r#"{"running":false,"model":"m"}"#).expect("old live state loads");
        assert!(old.context.is_none());
    }

    #[test]
    fn live_state_queued_is_omitted_when_empty_and_optional_on_read() {
        let empty = LiveState::default();
        let json = serde_json::to_value(&empty).unwrap();
        assert!(json.get("queued").is_none());

        let filled = LiveState {
            queued: vec!["next".into()],
            ..LiveState::default()
        };
        let json = serde_json::to_value(&filled).unwrap();
        assert_eq!(json["queued"], serde_json::json!(["next"]));
        let back: LiveState = serde_json::from_value(json).unwrap();
        assert_eq!(back.queued, vec!["next".to_string()]);

        // A LiveState from an older peer (no `queued` key) still loads.
        let old: LiveState =
            serde_json::from_str(r#"{"running":true,"model":"m"}"#).expect("old live state loads");
        assert!(old.running);
        assert!(old.queued.is_empty());
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
                        call_id: None,
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
                    agent: None,
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
                    kind: None,
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
                call_id: None,
            },
            ToolActivity::Start {
                name: "read".into(),
                call_id: Some("call_1".into()),
            },
            ToolActivity::Done {
                name: "bash".into(),
                is_error: true,
                bytes: 4_000_000_000usize,
                call_id: None,
            },
            ToolActivity::Done {
                name: "bash".into(),
                is_error: false,
                bytes: 7,
                call_id: Some("call_1".into()),
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
            ToolActivity::SubagentState {
                id: "a".into(),
                label: "explore".into(),
                state: AgentState::WaitingApproval,
            },
            ToolActivity::BackgroundFinished {
                id: "a".into(),
                label: "explore".into(),
                ok: false,
                brief: "stopped".into(),
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
    fn tool_activity_call_id_is_optional_on_the_wire() {
        // Pre-LM JSON (no call_id) still parses, to call_id None.
        let old_start: WireToolActivity =
            serde_json::from_str(r#"{"kind":"start","name":"read"}"#).unwrap();
        assert_eq!(
            old_start,
            WireToolActivity::Start {
                name: "read".into(),
                call_id: None,
            }
        );
        let old_done: WireToolActivity =
            serde_json::from_str(r#"{"kind":"done","name":"read","is_error":false,"bytes":3}"#)
                .unwrap();
        assert_eq!(
            ToolActivity::from(old_done),
            ToolActivity::Done {
                name: "read".into(),
                is_error: false,
                bytes: 3,
                call_id: None,
            }
        );
        // None is never serialized, so old peers see the exact old shape.
        let json = serde_json::to_string(&WireToolActivity::Start {
            name: "read".into(),
            call_id: None,
        })
        .unwrap();
        assert_eq!(json, r#"{"kind":"start","name":"read"}"#);
        // Some round-trips and is serialized.
        let with_id = WireToolActivity::Done {
            name: "read".into(),
            is_error: false,
            bytes: 3,
            call_id: Some("call_9".into()),
        };
        let json = serde_json::to_string(&with_id).unwrap();
        assert!(json.contains(r#""call_id":"call_9""#), "{json}");
        assert_eq!(
            serde_json::from_str::<WireToolActivity>(&json).unwrap(),
            with_id
        );
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
    fn agent_frames_roundtrip_with_tags() {
        let session = Uuid::new_v4();
        let steer = Frame::request(
            3,
            Payload::SteerAgent {
                session_id: session,
                agent: "a1".into(),
                text: "narrow it".into(),
            },
        );
        assert_roundtrip(&steer);
        let json = serde_json::to_value(&steer).unwrap();
        assert_eq!(json["type"], "steer_agent");
        assert_eq!(json["agent"], "a1");

        let kill = Frame::request(
            4,
            Payload::KillAgent {
                session_id: session,
                agent: "a1".into(),
            },
        );
        assert_roundtrip(&kill);
        assert_eq!(serde_json::to_value(&kill).unwrap()["type"], "kill_agent");

        let finished = Frame::event(
            Some(session),
            7,
            Payload::AgentFinished {
                agent: "a1".into(),
                ok: false,
                brief: "stopped".into(),
            },
        );
        assert_roundtrip(&finished);
        let json = serde_json::to_value(&finished).unwrap();
        assert_eq!(json["type"], "agent_finished");
        assert_eq!(json["ok"], false);
        assert_eq!(json["brief"], "stopped");
    }

    #[test]
    fn swarm_row_state_is_additive_on_the_wire() {
        let row = WireSwarmRow {
            id: "a1".into(),
            label: "delegate".into(),
            turns: 0,
            tokens: 0,
            last_tool: None,
            done: None,
            state: AgentState::WaitingApproval,
        };
        let json = serde_json::to_value(&row).unwrap();
        assert_eq!(json["state"], "waiting_approval");
        assert_eq!(
            serde_json::from_value::<WireSwarmRow>(json).unwrap(),
            row,
            "round trip"
        );

        // A row from an older server has no state: it reads as running and
        // keeps its done flag.
        let old: WireSwarmRow = serde_json::from_value(serde_json::json!({
            "id": "a1",
            "label": "explore",
            "turns": 2,
            "tokens": 5,
            "done": true
        }))
        .expect("old row loads");
        assert_eq!(old.state, AgentState::Running);
        assert_eq!(old.done, Some(true));
    }

    #[test]
    fn killed_state_and_background_settlement_serialize_snake_case() {
        let killed = WireToolActivity::SubagentState {
            id: "a1".into(),
            label: "delegate".into(),
            state: AgentState::Killed,
        };
        let json = serde_json::to_value(&killed).unwrap();
        assert_eq!(json["kind"], "subagent_state");
        assert_eq!(json["state"], "killed");
        let settled = WireToolActivity::BackgroundFinished {
            id: "a1".into(),
            label: "delegate".into(),
            ok: true,
            brief: "ok".into(),
        };
        assert_eq!(
            serde_json::to_value(&settled).unwrap()["kind"],
            "background_finished"
        );
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
    fn live_state_config_fields_round_trip() {
        // C5: the server's mode, sandbox and allow/deny lists ride the
        // snapshot so a fresh attach shows the real labels.
        let frame = Frame::event(
            None,
            1,
            Payload::Snapshot {
                session_id: Uuid::new_v4(),
                last_seq: 0,
                events: vec![],
                live: LiveState {
                    approval_mode: Some(ApprovalModeWire::ApproveAll),
                    sandbox: Some(SandboxPolicy::Bubblewrap),
                    allow: vec!["diagnostics".into()],
                    deny: vec!["bash".into(), "write".into()],
                    ..LiveState::default()
                },
            },
        );
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains("\"approval_mode\":\"approve_all\""));
        assert!(json.contains("\"sandbox\":\"bubblewrap\""));
        assert!(json.contains("\"allow\":[\"diagnostics\"]"));
        assert!(json.contains("\"deny\":[\"bash\",\"write\"]"));
        assert_roundtrip(&frame);
    }

    #[test]
    fn live_state_config_fields_are_omitted_when_unset() {
        // An unset LiveState writes none of the C5 keys, so an older
        // client sees exactly the bytes it always did.
        let value = serde_json::to_value(LiveState::default()).unwrap();
        let object = value.as_object().unwrap();
        for key in ["approval_mode", "sandbox", "allow", "deny"] {
            assert!(!object.contains_key(key), "{key} should be omitted");
        }
    }

    #[test]
    fn snapshot_without_config_fields_still_parses() {
        // A pre-C5 server's snapshot: no approval_mode, sandbox, allow or
        // deny. The fields default to unset / empty.
        let json = format!(
            r#"{{"seq":1,"type":"snapshot","session_id":"{u}","last_seq":0,"events":[],"live":{{"running":false,"model":"m","provider":"p"}}}}"#,
            u = Uuid::new_v4()
        );
        let frame: Frame = serde_json::from_str(&json).expect("old snapshot loads");
        match frame.payload {
            Payload::Snapshot { live, .. } => {
                assert_eq!(live.approval_mode, None);
                assert_eq!(live.sandbox, None);
                assert!(live.allow.is_empty());
                assert!(live.deny.is_empty());
                assert_eq!(live.model, "m");
            }
            other => panic!("expected snapshot, got {other:?}"),
        }
    }

    /// DecidePlan (client) and PlanProposed / PlanDecided (server) round-trip
    /// with snake_case tags; `note` is omitted when absent.
    #[test]
    fn plan_frames_roundtrip_with_tags() {
        let session = Uuid::new_v4();
        let decide = Frame::request(
            14,
            Payload::DecidePlan {
                session_id: session,
                decision: PlanDecision::Revise,
                note: Some("split step 2".into()),
            },
        );
        assert_roundtrip(&decide);
        let json = serde_json::to_value(&decide).unwrap();
        assert_eq!(json["type"], "decide_plan");
        assert_eq!(json["decision"], "revise");
        assert_eq!(json["note"], "split step 2");

        let bare = Frame::request(
            15,
            Payload::DecidePlan {
                session_id: session,
                decision: PlanDecision::Approve,
                note: None,
            },
        );
        assert_roundtrip(&bare);
        let json = serde_json::to_value(&bare).unwrap();
        assert_eq!(json["decision"], "approve");
        assert!(json.get("note").is_none());

        let items = vec![PlanItem {
            text: "add the parser".into(),
            done_criteria: "parser tests pass".into(),
            status: "pending".into(),
        }];
        let proposed = Frame::event(
            Some(session),
            16,
            Payload::PlanProposed {
                items: items.clone(),
            },
        );
        assert_roundtrip(&proposed);
        let json = serde_json::to_value(&proposed).unwrap();
        assert_eq!(json["type"], "plan_proposed");
        assert_eq!(json["items"][0]["text"], "add the parser");

        let decided = Frame::event(
            Some(session),
            17,
            Payload::PlanDecided {
                decision: PlanDecision::Approve,
            },
        );
        assert_roundtrip(&decided);
        let json = serde_json::to_value(&decided).unwrap();
        assert_eq!(json["type"], "plan_decided");
        assert_eq!(json["decision"], "approve");
    }

    /// The phase rides ConfigPatch and LiveState; unset, both stay off the
    /// wire, and older JSON without them still parses.
    #[test]
    fn phase_and_plan_pending_are_optional_on_the_wire() {
        let patch = ConfigPatch {
            phase: Some(Phase::Plan),
            ..ConfigPatch::default()
        };
        let json = serde_json::to_value(&patch).unwrap();
        assert_eq!(json["phase"], "plan");
        let back: ConfigPatch = serde_json::from_value(json).unwrap();
        assert_eq!(back.phase, Some(Phase::Plan));

        let unset = serde_json::to_value(ConfigPatch::default()).unwrap();
        assert!(unset.get("phase").is_none());
        let old: ConfigPatch = serde_json::from_str(r#"{"model":"m"}"#).expect("old patch loads");
        assert_eq!(old.phase, None);
        assert_eq!(old.model.as_deref(), Some("m"));

        let live = LiveState {
            phase: Some(Phase::Build),
            plan_pending: true,
            ..LiveState::default()
        };
        let json = serde_json::to_value(&live).unwrap();
        assert_eq!(json["phase"], "build");
        assert_eq!(json["plan_pending"], true);
        let back: LiveState = serde_json::from_value(json).unwrap();
        assert_eq!(back.phase, Some(Phase::Build));
        assert!(back.plan_pending);

        let empty = serde_json::to_value(LiveState::default()).unwrap();
        assert!(empty.get("phase").is_none());
        assert!(empty.get("plan_pending").is_none());

        // A LiveState / ConfigPatch from an older peer (no plan fields).
        let old_live: LiveState =
            serde_json::from_str(r#"{"running":false,"model":"m"}"#).expect("old live loads");
        assert_eq!(old_live.phase, None);
        assert!(!old_live.plan_pending);
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

    /// Permission rules ride ConfigPatch (replace-all), LiveState (report)
    /// and ExplainRule (client request, correlated Note reply). Unset rules
    /// stay off the wire, and older JSON without them still parses.
    #[test]
    fn rules_frames_roundtrip_and_stay_optional() {
        let session = Uuid::new_v4();
        let rule = |effect: &str, spec: &str| RuleWire {
            effect: effect.into(),
            spec: spec.into(),
        };

        let patch = ConfigPatch {
            rules: Some(vec![rule("deny", "bash(rm -rf*)"), rule("allow", "read")]),
            ..ConfigPatch::default()
        };
        let set = Frame::request(
            21,
            Payload::SetConfig {
                patch: patch.clone(),
            },
        );
        assert_roundtrip(&set);
        let json = serde_json::to_value(&set).unwrap();
        assert_eq!(json["patch"]["rules"][0]["effect"], "deny");
        assert_eq!(json["patch"]["rules"][0]["spec"], "bash(rm -rf*)");
        assert_eq!(json["patch"]["rules"][1]["spec"], "read");

        // An absent rules field is "leave alone", and an empty list is a
        // real clear: the two must stay distinct on the wire.
        let unset = serde_json::to_value(ConfigPatch::default()).unwrap();
        assert!(unset.get("rules").is_none());
        let cleared = serde_json::to_value(ConfigPatch {
            rules: Some(Vec::new()),
            ..ConfigPatch::default()
        })
        .unwrap();
        assert_eq!(cleared["rules"], serde_json::json!([]));

        let live = LiveState {
            rules: vec![rule("ask", "bash(git push*)")],
            ..LiveState::default()
        };
        let json = serde_json::to_value(&live).unwrap();
        assert_eq!(json["rules"][0]["effect"], "ask");
        let back: LiveState = serde_json::from_value(json).unwrap();
        assert_eq!(back, live);
        let empty = serde_json::to_value(LiveState::default()).unwrap();
        assert!(empty.get("rules").is_none());

        let explain = Frame::request(
            22,
            Payload::ExplainRule {
                session_id: session,
                tool: "bash".into(),
                args: serde_json::json!({"command": "rm -rf x"}),
            },
        );
        assert_roundtrip(&explain);
        let json = serde_json::to_value(&explain).unwrap();
        assert_eq!(json["type"], "explain_rule");
        assert_eq!(json["tool"], "bash");
        assert_eq!(json["args"]["command"], "rm -rf x");

        // `args` may be omitted; it then reads as JSON null.
        let bare: Payload = serde_json::from_value(serde_json::json!({
            "type": "explain_rule",
            "session_id": session,
            "tool": "read",
        }))
        .unwrap();
        match bare {
            Payload::ExplainRule { args, tool, .. } => {
                assert_eq!(tool, "read");
                assert!(args.is_null());
            }
            _ => panic!("expected explain_rule"),
        }

        let note = Frame::event(
            Some(session),
            23,
            Payload::Note {
                text: "allow read: matched\nresult: allow".into(),
            },
        );
        assert_roundtrip(&note);
    }

    /// JSON written before the rules fields existed (a SetConfig without
    /// `rules`, a Snapshot LiveState without `rules`) still deserializes.
    #[test]
    fn rules_absent_from_older_json_still_parses() {
        let patch: ConfigPatch =
            serde_json::from_str(r#"{"model":"m1","approval_mode":"ask"}"#).unwrap();
        assert_eq!(patch.model.as_deref(), Some("m1"));
        assert_eq!(patch.rules, None);

        let live: LiveState =
            serde_json::from_str(r#"{"running":false,"model":"m1","provider":"mock"}"#).unwrap();
        assert!(live.rules.is_empty());
    }

    /// H3b: `agent` is additive. Old JSON (no field) parses as a top-level
    /// ask; a child's ask keeps its label through a roundtrip.
    #[test]
    fn approval_requested_agent_is_additive() {
        let old: Payload = serde_json::from_str(
            r#"{"type":"approval_requested","request_id":"r1",
                "call":{"id":"c1","name":"edit","arguments":{}}}"#,
        )
        .unwrap();
        match old {
            Payload::ApprovalRequested { agent, .. } => assert!(agent.is_none()),
            other => panic!("unexpected payload {other:?}"),
        }
        let child = Payload::ApprovalRequested {
            request_id: "r2".into(),
            call: ToolCall {
                id: "c2".into(),
                name: "bash".into(),
                arguments: serde_json::json!({}),
            },
            agent: Some("delegate".into()),
        };
        let json = serde_json::to_value(&child).unwrap();
        assert_eq!(json["agent"], "delegate");
        let top = serde_json::json!({"type":"approval_requested","request_id":"r3",
            "call":{"id":"c3","name":"bash","arguments":{}}});
        let back: Payload = serde_json::from_value(top).unwrap();
        let reserialized = serde_json::to_value(&back).unwrap();
        assert!(reserialized.get("agent").is_none(), "absent when None");
    }

    /// H7c: the roles fields round-trip, unset roles stay off the wire, an
    /// empty roles value clears, and older peers' JSON (no roles) parses.
    #[test]
    fn roles_fields_round_trip_and_stay_optional_on_the_wire() {
        let roles = ModelRoles {
            plan: Some("plan-model".into()),
            subagent: None,
            critic: None,
            summary: Some("summary-model".into()),
        };
        let patch = ConfigPatch {
            roles: Some(Box::new(roles.clone())),
            ..ConfigPatch::default()
        };
        let json = serde_json::to_value(&patch).unwrap();
        assert_eq!(json["roles"]["plan"], "plan-model");
        assert!(
            json["roles"].get("critic").is_none(),
            "unset roles are omitted"
        );
        let back: ConfigPatch = serde_json::from_value(json).unwrap();
        assert_eq!(back.roles, Some(Box::new(roles.clone())));

        let unset = serde_json::to_value(ConfigPatch::default()).unwrap();
        assert!(
            unset.get("roles").is_none(),
            "absent roles stay off the wire"
        );
        let clear = serde_json::to_value(ConfigPatch {
            roles: Some(Box::new(ModelRoles::default())),
            ..ConfigPatch::default()
        })
        .unwrap();
        assert_eq!(clear["roles"], serde_json::json!({}));
        let old: ConfigPatch = serde_json::from_value(serde_json::json!({"model": "m"})).unwrap();
        assert_eq!(old.roles, None);

        let empty_live = serde_json::to_value(LiveState::default()).unwrap();
        assert!(empty_live.get("roles").is_none());
        let filled = LiveState {
            roles,
            ..LiveState::default()
        };
        let json = serde_json::to_value(&filled).unwrap();
        assert_eq!(json["roles"]["summary"], "summary-model");
        let back: LiveState = serde_json::from_value(json).unwrap();
        assert_eq!(back.roles.summary.as_deref(), Some("summary-model"));
        let old_live: LiveState = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(old_live.roles.is_empty());
    }

    /// H7a: the effort fields round-trip, an unset patch stays off the wire,
    /// and an older peer's JSON (no effort keys) still parses.
    #[test]
    fn effort_fields_round_trip_and_stay_optional_on_the_wire() {
        let patch = ConfigPatch {
            effort: Some(Effort::Xhigh),
            ..ConfigPatch::default()
        };
        let json = serde_json::to_value(&patch).unwrap();
        assert_eq!(json["effort"], "xhigh");
        assert!(json.get("clear_effort").is_none(), "false is omitted");
        let back: ConfigPatch = serde_json::from_value(json).unwrap();
        assert_eq!(back, patch);

        let clear = ConfigPatch {
            clear_effort: true,
            ..ConfigPatch::default()
        };
        let json = serde_json::to_value(&clear).unwrap();
        assert_eq!(json["clear_effort"], true);
        assert!(json.get("effort").is_none());

        let old: ConfigPatch = serde_json::from_value(serde_json::json!({"model": "m"})).unwrap();
        assert_eq!(old.effort, None);
        assert!(!old.clear_effort);
    }

    #[test]
    fn live_state_effort_is_omitted_when_unset_and_optional_on_read() {
        let empty = serde_json::to_value(LiveState::default()).unwrap();
        assert!(empty.get("effort").is_none());
        let filled = LiveState {
            effort: Some(Effort::Max),
            ..LiveState::default()
        };
        let json = serde_json::to_value(&filled).unwrap();
        assert_eq!(json["effort"], "max");
        let back: LiveState = serde_json::from_value(json).unwrap();
        assert_eq!(back.effort, Some(Effort::Max));
        let old: LiveState = serde_json::from_value(serde_json::json!({"running": false})).unwrap();
        assert_eq!(old.effort, None);
    }
}
