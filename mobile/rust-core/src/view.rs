//! The mobile view-model: one serde-friendly projection of a session that
//! a native shell (nsh TSX) or a future Swift/Kotlin bridge renders by
//! polling. All strings a screen shows are produced here, from the same
//! modron-view primitives the TUI and desktop use — per the AGENTS.md
//! parity contract, no client formats anything locally.
//!
//! Folding rules (one defined effect per protocol event):
//! - `Snapshot` resets: `replay(snapshot.events)` replaces history, the
//!   live half seeds streaming buffers, the tool tracker, the pending
//!   approval, and cumulative usage.
//! - `Delta::Text` appends `streaming_text`; `Delta::Reasoning` appends
//!   `streaming_reasoning` only when `show_reasoning` is on (invariant 5:
//!   reasoning streams only when the user opted in). Buffers commit to
//!   history at `TurnDone` and clear (uncommitted) on abort/failure —
//!   TUI streaming parity.
//! - `Event` frames append committed rows by folding through
//!   [`modron_view::ReplayFolder`] one push at a time (see `FoldState`).
//! - `Tool` frames feed [`modron_view::ActivityTracker`] (the live strip).
//! - `ApprovalRequested` joins a FIFO queue; the view shows the front.
//!   `ApprovalResolved` removes its request id wherever it sits in the
//!   queue; answering removes the front. A `Snapshot` replaces the queue
//!   with the snapshot's own pending approval; a disconnect clears it.
//!   Config changes are label updates for future turns only.
//! - `Steered` queues a pending steer echo; the persisted `Task` event with
//!   the same text removes it; turn end and a Snapshot clear the rest.
//! - `QueueChanged` replaces the queue; `ContextChanged` sets the context
//!   gauge; `PlanProposed` opens the plan block (lines from
//!   `modron_view::plan_lines`), `PlanDecided` clears it. A Snapshot sets
//!   the plan block from `LiveState.plan_pending` (no lines on attach).
//! - `ConfigChanged` `phase` sets the phase label, `rules` replaces the
//!   rule list; a Snapshot sets all of them from `LiveState`.
//!
//! Usage accounting: cumulative usage grows from persisted `Usage` events
//! (`usage_totals` arithmetic) and resets to the snapshot's live value on
//! attach, so a reconnect never double-counts. `TurnDone` summaries do
//! not add usage (the server's `Usage` events already covered the turn).

use modron_protocol::{
    ApprovalModeWire, ContextGaugeWire, Decision, LiveState, PendingApproval, PlanDecision,
    RuleWire, SessionMetaWire, WireRunSummary,
};
use modron_types::{Effort, Phase, PlanItem, Role, SessionEvent, ToolActivity, ToolCall, Usage};
use modron_view::{
    compact_tokens, replay, ActivityTracker, ReplayFolder, SwarmRow, ToolEvent, TranscriptEntry,
};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Parity constants — the exact shared words (AGENTS.md UI/UX parity contract)
// ---------------------------------------------------------------------------

/// Decision labels for the approval sheet, in protocol order. The shared
/// words (AGENTS.md invariant 1, `allow once / allow all / deny`), same
/// as the desktop's `decision_label`; the TUI's strip adds key hints
/// (`y = allow once · a = allow for this run · n/Esc = deny`) which are
/// TUI presentation only — keys differ per the divergence table.
pub const DECISION_LABELS: [(Decision, &str); 3] = [
    (Decision::AllowOnce, Decision::AllowOnce.label()),
    (Decision::AllowAll, Decision::AllowAll.label()),
    (Decision::Deny, Decision::Deny.label()),
];

/// The plan decision labels for the plan decision block, in protocol
/// order. Shared words for every client (the plan block is the approval
/// surface's sibling: `approve` flips to build, `revise` plans again).
pub const PLAN_DECISION_LABELS: [(PlanDecision, &str); 2] = [
    (PlanDecision::Approve, "approve"),
    (PlanDecision::Revise, "revise"),
];

/// The reasoning-effort label for the status line (`effort high`): empty
/// when unset, so the shell shows nothing for the provider default.
pub fn effort_label(effort: Option<Effort>) -> String {
    effort.map(|e| e.label().to_string()).unwrap_or_default()
}

/// The run-phase label for the status line and settings (`plan` / `build`).
pub fn phase_label(phase: Phase) -> &'static str {
    match phase {
        Phase::Plan => "plan",
        Phase::Build => "build",
    }
}

/// The context meter label (`ctx 41% (74k/180k)`). Empty when the gauge
/// is absent or its window is zero (no meaningful percentage), so the
/// shell hides the meter.
pub fn context_label(gauge: Option<ContextGaugeWire>) -> String {
    match gauge {
        Some(ContextGaugeWire { used, window }) if window > 0 => {
            // Rounded percentage, computed in integers.
            let pct = used.saturating_mul(100).saturating_add(window / 2) / window;
            format!(
                "ctx {pct}% ({}/{})",
                compact_tokens(used),
                compact_tokens(window)
            )
        }
        _ => String::new(),
    }
}

/// The approval mode label for the status line (`ask`/`yolo`/`deny`,
/// default `ask`), delegating to the shared protocol label.
pub fn approval_mode_label(mode: Option<ApprovalModeWire>) -> &'static str {
    mode.unwrap_or(ApprovalModeWire::Ask).label()
}

/// The sandbox label for the settings views (`off` / `seatbelt` /
/// `bwrap`), delegating to the shared modron-view label.
pub fn sandbox_label(policy: modron_types::SandboxPolicy) -> &'static str {
    modron_view::sandbox_label(policy)
}

/// Connection-state words for the mobile header.
pub fn connection_label(state: &ConnectionState) -> &'static str {
    match state {
        ConnectionState::Connected => "connected",
        ConnectionState::Connecting => "connecting",
        ConnectionState::Disconnected => "disconnected",
    }
}

// ---------------------------------------------------------------------------
// Sessions and settings copy (the NSH app's screens and any later shell)
// ---------------------------------------------------------------------------

/// Navigation and screen words.
pub const SESSIONS_LABEL: &str = "sessions";
pub const SETTINGS_LABEL: &str = "settings";
pub const NEW_SESSION_LABEL: &str = "new session";
pub const SESSIONS_EMPTY_LABEL: &str = "no sessions yet";
pub const SESSIONS_NO_MATCH_LABEL: &str = "no sessions match";
pub const FILTER_LABEL: &str = "filter";
/// The running/idle words, for a session row and the status surface.
pub const RUNNING_LABEL: &str = "running";
pub const IDLE_LABEL: &str = "idle";
/// Settings row labels, in screen order.
pub const SETTING_MODEL_LABEL: &str = "model";
pub const SETTING_PROVIDER_LABEL: &str = "provider";
pub const SETTING_APPROVAL_LABEL: &str = "approval mode";
pub const SETTING_PHASE_LABEL: &str = "phase";
pub const SETTING_EFFORT_LABEL: &str = "effort";
pub const SETTING_SANDBOX_LABEL: &str = "sandbox";
pub const SETTING_CONTEXT_LABEL: &str = "context";
pub const SETTING_RULES_LABEL: &str = "rules";
pub const SETTING_COMPACT_LABEL: &str = "compact";
/// Shown as the value of an unset effort, and of an empty rule list.
pub const EFFORT_UNSET_LABEL: &str = "unset";
pub const RULES_NONE_LABEL: &str = "none";

/// One sessions-list row: the title (the short id when untitled), the
/// running/idle word and the message count. Shells show it verbatim.
pub fn session_row(meta: &SessionMetaWire, running: bool) -> String {
    let title = if meta.title.trim().is_empty() {
        meta.id.to_string().chars().take(8).collect()
    } else {
        meta.title.clone()
    };
    let state = if running { RUNNING_LABEL } else { IDLE_LABEL };
    format!(
        "{title} \u{b7} {state} \u{b7} {} messages",
        meta.message_count
    )
}

/// The next approval mode in the settings cycle: ask, yolo, deny, ask.
pub fn approval_mode_next(mode: Option<ApprovalModeWire>) -> ApprovalModeWire {
    match mode.unwrap_or(ApprovalModeWire::Ask) {
        ApprovalModeWire::Ask => ApprovalModeWire::ApproveAll,
        ApprovalModeWire::ApproveAll => ApprovalModeWire::Deny,
        ApprovalModeWire::Deny => ApprovalModeWire::Ask,
    }
}

/// The next reasoning effort: unset, low, medium, high, xhigh, max, unset.
pub fn effort_next(effort: Option<Effort>) -> Option<Effort> {
    match effort {
        None => Some(Effort::Low),
        Some(Effort::Low) => Some(Effort::Medium),
        Some(Effort::Medium) => Some(Effort::High),
        Some(Effort::High) => Some(Effort::Xhigh),
        Some(Effort::Xhigh) => Some(Effort::Max),
        Some(Effort::Max) => None,
    }
}

/// The other run phase: build and plan swap.
pub fn phase_next(phase: Phase) -> Phase {
    match phase {
        Phase::Build => Phase::Plan,
        Phase::Plan => Phase::Build,
    }
}

/// The next sandbox policy: off, seatbelt, bwrap, off.
pub fn sandbox_next(policy: modron_types::SandboxPolicy) -> modron_types::SandboxPolicy {
    use modron_types::SandboxPolicy as S;
    match policy {
        S::Off => S::Seatbelt,
        S::Seatbelt => S::Bubblewrap,
        S::Bubblewrap => S::Off,
    }
}

/// One pre-rendered transcript row for shells that cannot call Rust
/// helpers: the shared copy verbatim.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RenderedRow {
    /// The `TranscriptEntry` this row renders (typed consumers re-derive).
    pub entry: TranscriptEntry,
    /// The shared-format strings to display (usually one line).
    pub lines: Vec<String>,
}

/// Render one committed transcript row with the shared words: task echo
/// (`❯` prefix), assistant/user text, tool summary lines, swarm rows, the
/// TUI's token/cache usage arithmetic, model and compaction notes.
pub fn entry_lines(entry: &TranscriptEntry) -> Vec<String> {
    match entry {
        TranscriptEntry::Task { text } => vec![format!("❯ {text}")],
        TranscriptEntry::User { text } => vec![text.clone()],
        TranscriptEntry::Assistant { text } => vec![text.clone()],
        TranscriptEntry::ToolSummary { line, .. } => vec![line.clone()],
        TranscriptEntry::Swarm { lines } => lines.clone(),
        TranscriptEntry::Model { model, reason } => vec![format!("model: {model} ({reason})")],
        TranscriptEntry::Usage { usage, .. } => {
            // Same arithmetic and words as the TUI's per-turn usage note
            // (`{total} tok · cache {pct}%`); the turn count lives in the
            // status line on mobile instead. The cache percentage is the
            // shared `cache_hit_pct` (the canonical arithmetic).
            let cache = match modron_view::cache_hit_pct(usage) {
                Some(pct) => format!("cache {pct:.0}%"),
                None => "cache —".to_string(),
            };
            vec![format!("{} tok · {}", usage.total(), cache)]
        }
        TranscriptEntry::Compaction {
            summary,
            summarized_events,
        } => vec![format!("compacted {summarized_events} events: {summary}")],
        TranscriptEntry::GenerativeUi { ui } => modron_view::render_ascii(ui),
        TranscriptEntry::Plan { items } => modron_view::plan_lines(items),
        // Reviewer verdict: the shared one-line summary, then the shared
        // finding rows (same words as every client).
        TranscriptEntry::Review {
            summary, findings, ..
        } => std::iter::once(summary.clone())
            .chain(findings.iter().map(|f| format!("  {f}")))
            .collect(),
        // Generative widgets render as ASCII on text surfaces (same
        // projection the TUI uses).
    }
}

// ---------------------------------------------------------------------------
// Pending approval view
// ---------------------------------------------------------------------------

/// The approval awaiting a decision, pre-summarized for the sheet.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingApprovalView {
    pub request_id: String,
    /// Tool name.
    pub tool: String,
    /// One-line summary: `modron_view::approval_summary` (essential
    /// argument first, `[+N chars]` when truncated), shared with the TUI
    /// and desktop.
    pub args_summary: String,
    /// Full parsed arguments for the detail sheet.
    pub arguments: serde_json::Value,
    /// The full call as sanitized compact JSON, for the sheet's expandable
    /// block (never truncated; bidi/control characters replaced).
    pub arguments_text: String,
    /// The three decisions with their shared labels, protocol order.
    pub options: Vec<DecisionOption>,
}

/// One selectable decision on the approval sheet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionOption {
    pub decision: Decision,
    /// Shared label (`allow once` / `allow all` / `deny`).
    pub label: String,
}

impl PendingApprovalView {
    pub fn new(request_id: String, call: &ToolCall) -> Self {
        Self::for_agent(request_id, call, None)
    }

    /// H3b: a delegated child's ask shows as `[agent] <summary>`; the
    /// top-level run's own calls (`agent: None`) keep the bare summary.
    pub fn for_agent(request_id: String, call: &ToolCall, agent: Option<&str>) -> Self {
        let full = serde_json::to_string(&call.arguments).unwrap_or_default();
        let summary = modron_view::approval_summary(
            &call.name,
            &call.arguments,
            modron_view::APPROVAL_SUMMARY_MAX,
        );
        Self {
            request_id,
            tool: call.name.clone(),
            args_summary: match agent {
                Some(agent) => format!("[{agent}] {summary}"),
                None => summary,
            },
            arguments: call.arguments.clone(),
            arguments_text: modron_view::sanitize_display(&full),
            options: DECISION_LABELS
                .iter()
                .map(|(d, label)| DecisionOption {
                    decision: *d,
                    label: (*label).to_string(),
                })
                .collect(),
        }
    }
}

/// One selectable decision on the plan block.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanDecisionOption {
    pub decision: PlanDecision,
    /// Shared label (`approve` / `revise`).
    pub label: String,
}

/// A proposed plan awaiting a decision (`PlanProposed`), pre-rendered.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingPlanView {
    /// The plan as checkbox rows (`modron_view::plan_lines`). Empty when
    /// the plan came from a Snapshot: its `LiveState` carries only the
    /// pending flag, not the items.
    pub lines: Vec<String>,
    /// The two decisions with their shared labels, protocol order.
    pub options: [PlanDecisionOption; 2],
}

impl PendingPlanView {
    pub fn new(items: &[PlanItem]) -> Self {
        Self {
            lines: modron_view::plan_lines(items),
            options: PLAN_DECISION_LABELS.map(|(decision, label)| PlanDecisionOption {
                decision,
                label: label.to_string(),
            }),
        }
    }
}

/// OAuth sign-in progress surfaced to remote clients.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthProgressView {
    pub provider: String,
    pub stage: String,
    pub message: String,
    pub url: Option<String>,
    pub user_code: Option<String>,
}

// ---------------------------------------------------------------------------
// Connection state
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ConnectionState {
    #[default]
    Disconnected,
    Connecting,
    Connected,
}

// ---------------------------------------------------------------------------
// View model + snapshot
// ---------------------------------------------------------------------------

/// The foldable session view behind the client's mutex. `history` is the
/// committed transcript; `new_entries` is the drain queue for polling UIs.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionViewModel {
    pub session_id: Option<Uuid>,
    pub connection: ConnectionState,
    pub server_version: String,
    pub running: bool,
    pub streaming_text: String,
    pub streaming_reasoning: String,
    /// Whether streamed reasoning surfaces at all (AGENTS.md streaming
    /// invariant 5: reasoning streams only when the user opted in — the
    /// TUI's `/reasoning` and the desktop's reasoning chip, default off).
    /// While off, the fold drops reasoning deltas and the snapshot seeds
    /// an empty buffer.
    pub show_reasoning: bool,
    /// Live grouped tool strip (`ToolEvent::summary()` rows).
    pub turn_tools: Vec<ToolEvent>,
    /// Live swarm rows.
    pub subagents: Vec<SwarmRow>,
    /// Approvals awaiting a decision, oldest first. The view shows the
    /// front; see the module docs for the queue rules.
    pub pending_approvals: VecDeque<PendingApprovalView>,
    /// Inbound frames this client could not decode (counted, not shown).
    pub undecodable_frames: u64,
    pub cumulative_usage: Usage,
    pub model: String,
    pub provider: String,
    pub goal: String,
    pub approval_mode: Option<ApprovalModeWire>,
    /// Sandbox for tool execution (the last config patch; the settings
    /// views cycle it).
    pub sandbox: modron_types::SandboxPolicy,
    /// Tool allow/deny lists (the last config patches).
    pub allow_tools: Vec<String>,
    pub deny_tools: Vec<String>,
    /// Messages held for the next run, front first (`QueueChanged`
    /// replaces it; a Snapshot seeds it from `LiveState.queued`).
    pub queued: Vec<String>,
    /// Steered texts accepted but not yet persisted as a task echo. Each
    /// is removed when its persisted `Task` event arrives; turn end and
    /// a Snapshot clear the lot.
    pub pending_steers: Vec<String>,
    /// A proposed plan awaiting a decision (`PlanProposed` sets it,
    /// `PlanDecided` clears it).
    pub plan_pending: Option<PendingPlanView>,
    /// The run phase for future runs (`ConfigPatch.phase`, snapshot).
    /// Defaults to `build`.
    pub phase: Phase,
    /// The reasoning effort for future turns (`ConfigPatch.effort` /
    /// `clear_effort`, snapshot). `None` = provider default, no label.
    pub effort: Option<Effort>,
    /// The context occupancy (`ContextChanged`, snapshot); `None` hides
    /// the meter.
    pub context: Option<ContextGaugeWire>,
    /// Permission rules in evaluation order (`ConfigPatch.rules`,
    /// snapshot).
    pub rules: Vec<RuleWire>,
    /// Sessions list (answers to `list_sessions`).
    pub sessions: Vec<SessionMetaWire>,
    /// Scheduled automations (answers to `list_automations`; settings
    /// views render these).
    pub automations: Vec<modron_protocol::AutomationWire>,
    /// Provider auth rows (answers to `auth_providers`; the settings
    /// providers card renders these).
    pub providers: Vec<modron_protocol::AuthProviderWire>,
    /// The router's default-model picture (answers to
    /// `router_defaults_req`; the settings defaults card renders it).
    pub router_defaults: Option<modron_protocol::RouterDefaults>,
    /// Last seen server seq for this session (reconnect resumes past it).
    pub last_seq: u64,
    pub turns_completed: u32,
    pub tool_calls_total: u32,
    /// Last `Error` frame or disconnect cause (transient toast material).
    pub last_error: Option<String>,
    /// Last `Note` frame text (slash-command results).
    pub last_note: Option<String>,
    pub oauth: Option<OAuthProgressView>,
    /// Committed transcript, in order, pre-rendered: the shared strings
    /// are produced once at commit time ([`commit`]), so a snapshot
    /// ([`ViewModelSnapshot::from_vm`], polled 4x/sec by the shell)
    /// clones finished rows instead of re-rendering every entry.
    pub history: Vec<RenderedRow>,
    /// Entries not yet drained by `take_new_entries`.
    pub new_entries: Vec<TranscriptEntry>,
}

/// Committed-transcript bound: both `history` and the `new_entries`
/// drain queue drop their oldest rows past this (the poll reads the
/// whole snapshot and nothing else drains the queue, so an uncapped
/// transcript would grow without bound on a long session).
const HISTORY_CAP: usize = 500;

/// Drop the oldest rows past [`HISTORY_CAP`] (FIFO; the display is the
/// most recent transcript either way).
fn cap_rows<T>(rows: &mut Vec<T>) {
    if rows.len() > HISTORY_CAP {
        rows.drain(..rows.len() - HISTORY_CAP);
    }
}

/// Render one committed row once: the typed entry plus its shared
/// strings ([`entry_lines`]).
fn rendered_row(entry: &TranscriptEntry) -> RenderedRow {
    RenderedRow {
        entry: entry.clone(),
        lines: entry_lines(entry),
    }
}

impl SessionViewModel {
    /// The collapsed turn-tool line (`⏺ 21 tools · glob ×9 · read ×8 …`),
    /// straight from modron-view.
    pub fn turn_summary_line(&self) -> Option<String> {
        (!self.turn_tools.is_empty()).then(|| modron_view::tool_summary_line(&self.turn_tools))
    }
}

/// Cheap clone of the view state plus every string a shell renders
/// verbatim (TSX displays `status_line`/`tool_lines`/rows as-is).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ViewModelSnapshot {
    pub connection: ConnectionState,
    pub connection_label: String,
    pub session_id: Option<Uuid>,
    pub server_version: String,
    pub running: bool,
    /// `running` or `idle`.
    pub running_label: String,
    /// `<model> · <tok> tok · cache <pct>% · <mode>` — the TUI status
    /// strip words (spinner and key hints are TUI presentation only).
    pub status_line: String,
    pub streaming_text: String,
    pub streaming_reasoning: String,
    /// Whether reasoning streaming is opted in (default off; mirrored
    /// from the view model so shells can render the toggle's state).
    pub show_reasoning: bool,
    /// Per-event rows (`✓ read — 2.1 KB` / `● bash …`).
    pub tool_lines: Vec<String>,
    /// The collapsed per-turn line (`⏺ 21 tools · glob ×9 …`), if any.
    pub turn_summary_line: Option<String>,
    /// Live swarm rows (`⏵ [label] done · 3 turns · …`).
    pub subagent_rows: Vec<String>,
    pub pending_approval: Option<PendingApprovalView>,
    pub approval_mode: Option<ApprovalModeWire>,
    /// `ask` / `yolo` / `deny`.
    pub approval_mode_label: String,
    /// `off` / `seatbelt` / `bwrap` (the desktop's sandbox words).
    pub sandbox: String,
    /// The sandbox policy itself (the settings cycle sends the next one).
    pub sandbox_policy: modron_types::SandboxPolicy,
    /// The run phase and the reasoning effort as values (the settings
    /// cycles start from them; the labels above are for display).
    pub phase: Phase,
    pub effort: Option<Effort>,
    /// One formatted row per `sessions` entry, in the same order
    /// ([`session_row`]). The sessions screen shows these verbatim.
    pub session_rows: Vec<String>,
    /// Tool allow/deny lists (editable in the settings views).
    pub allow_tools: Vec<String>,
    pub deny_tools: Vec<String>,
    /// Messages held for the next run, front first (the queue chips).
    pub queued: Vec<String>,
    /// Steers accepted but not yet echoed as persisted tasks (dim echoes).
    pub pending_steers: Vec<String>,
    /// The proposed plan awaiting a decision (the plan decision block).
    pub plan_pending: Option<PendingPlanView>,
    /// `plan` / `build`.
    pub phase_label: String,
    /// `high` etc., empty when the effort is unset (see `effort_label`).
    pub effort_label: String,
    /// `ctx 41% (74k/180k)`, empty when the context is unknown.
    pub context_label: String,
    /// Permission rules as `effect spec` lines (the settings view).
    pub rules: Vec<String>,
    pub cumulative_usage: Usage,
    pub cumulative_tokens: u64,
    /// Session-cumulative cache hit rate, `None` before any tokens.
    pub session_cache_pct: Option<f64>,
    pub turns_completed: u32,
    pub tool_calls_total: u32,
    pub model: String,
    pub provider: String,
    pub goal: String,
    pub sessions: Vec<SessionMetaWire>,
    /// Scheduled automations (settings views render these).
    pub automations: Vec<modron_protocol::AutomationWire>,
    /// Provider auth rows (the settings providers card renders these).
    pub providers: Vec<modron_protocol::AuthProviderWire>,
    /// The router's default-model picture (the settings defaults card).
    pub router_defaults: Option<modron_protocol::RouterDefaults>,
    pub last_seq: u64,
    pub last_error: Option<String>,
    pub last_note: Option<String>,
    pub oauth: Option<OAuthProgressView>,
    /// Committed transcript, rendered (`RenderedRow.lines` is display-
    /// ready; `entry` is the typed row for rich shells).
    pub history: Vec<RenderedRow>,
}

impl ViewModelSnapshot {
    pub fn from_vm(vm: &SessionViewModel) -> Self {
        // The shared cache-hit arithmetic (None before any tokens).
        let session_cache_pct = modron_view::cache_hit_pct(&vm.cumulative_usage);
        let mut status_line = if vm.model.is_empty() {
            "modron".to_string()
        } else {
            vm.model.clone()
        };
        // The effort sits next to the model, and only when set (H7a).
        if let Some(effort) = vm.effort {
            status_line.push_str(&format!(" · effort {}", effort.label()));
        }
        status_line.push_str(&format!(" · {} tok", vm.cumulative_usage.total()));
        if let Some(pct) = session_cache_pct {
            status_line.push_str(&format!(" · cache {pct:.0}%"));
        }
        status_line.push_str(&format!(" · {}", approval_mode_label(vm.approval_mode)));
        // Phase shows only in plan (build is the quiet default); the
        // context meter shows whenever the window is known.
        if vm.phase == Phase::Plan {
            status_line.push_str(&format!(" · {}", phase_label(vm.phase)));
        }
        let context = context_label(vm.context);
        if !context.is_empty() {
            status_line.push_str(&format!(" · {context}"));
        }
        Self {
            connection: vm.connection,
            connection_label: connection_label(&vm.connection).to_string(),
            session_id: vm.session_id,
            server_version: vm.server_version.clone(),
            running: vm.running,
            running_label: if vm.running { "running" } else { "idle" }.to_string(),
            status_line,
            streaming_text: vm.streaming_text.clone(),
            streaming_reasoning: vm.streaming_reasoning.clone(),
            show_reasoning: vm.show_reasoning,
            tool_lines: vm.turn_tools.iter().map(ToolEvent::summary).collect(),
            turn_summary_line: vm.turn_summary_line(),
            subagent_rows: vm.subagents.iter().map(SwarmRow::summary).collect(),
            pending_approval: vm.pending_approvals.front().cloned(),
            approval_mode: vm.approval_mode,
            approval_mode_label: approval_mode_label(vm.approval_mode).to_string(),
            sandbox: sandbox_label(vm.sandbox).to_string(),
            sandbox_policy: vm.sandbox,
            phase: vm.phase,
            effort: vm.effort,
            session_rows: vm
                .sessions
                .iter()
                .map(|meta| {
                    let running = meta.task_state.as_deref() == Some("running")
                        || (vm.running && vm.session_id == Some(meta.id));
                    session_row(meta, running)
                })
                .collect(),
            allow_tools: vm.allow_tools.clone(),
            deny_tools: vm.deny_tools.clone(),
            queued: vm.queued.clone(),
            pending_steers: vm.pending_steers.clone(),
            plan_pending: vm.plan_pending.clone(),
            phase_label: phase_label(vm.phase).to_string(),
            effort_label: effort_label(vm.effort),
            context_label: context_label(vm.context),
            rules: vm
                .rules
                .iter()
                .map(|rule| format!("{} {}", rule.effect, rule.spec))
                .collect(),
            cumulative_usage: vm.cumulative_usage,
            cumulative_tokens: vm.cumulative_usage.total(),
            session_cache_pct,
            turns_completed: vm.turns_completed,
            tool_calls_total: vm.tool_calls_total,
            model: vm.model.clone(),
            provider: vm.provider.clone(),
            goal: vm.goal.clone(),
            sessions: vm.sessions.clone(),
            automations: vm.automations.clone(),
            providers: vm.providers.clone(),
            router_defaults: vm.router_defaults.clone(),
            last_seq: vm.last_seq,
            last_error: vm.last_error.clone(),
            last_note: vm.last_note.clone(),
            oauth: vm.oauth.clone(),
            // Rows are rendered at commit time (`commit`/`fold_snapshot`);
            // this is a plain clone per poll, never a re-render. (The one
            // commit outside the per-event flow is the held ToolSummary
            // tail at the turn boundary — `FoldState::flush_tools` — and
            // it renders exactly the same once.)
            history: vm.history.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Incremental folding state (Event frames through ReplayFolder)
// ---------------------------------------------------------------------------

/// Bookkeeping for folding live `Event` frames: a
/// [`modron_view::ReplayFolder`] IS the replay algorithm, so `fold_event`
/// is a single `folder.push` (O(new rows), never a re-replay of the log)
/// and a live fold groups tools exactly like a whole-log `replay` by
/// construction. The mobile-only bits (streaming-buffer clears, usage
/// accumulation, the committed-tools guard) stay here, fed from the
/// entries each push returns.
///
/// The folder holds a trailing run of `ToolResult` events open (no
/// summary row yet) until the next boundary event — replay's own
/// end-of-log behavior. `flush_tools` mirrors that flush at the turn
/// boundary: the still-open group is re-folded through `replay` (the
/// naming prefix rides along so call ids recover their tool names), the
/// held summary commits like any other row, and the folder resets to the
/// prefix (the old `reset_to_prefix`) so the next turn starts clean.
#[derive(Default)]
pub(crate) struct FoldState {
    pub tracker: ActivityTracker,
    folder: ReplayFolder,
    /// The naming prefix: the most recent assistant `Message` (it carries
    /// `tool_calls`); replayed into a fresh folder after every reset.
    prefix: Vec<SessionEvent>,
    /// `ToolResult` events of the still-open tool group (the folder
    /// flushed it already once a boundary arrived). Empty at every
    /// boundary; the turn-end flush commits what it holds.
    open_group: Vec<SessionEvent>,
    /// Whether this turn already committed a tool summary (via the
    /// Event/ToolResult path); guards the live-strip commit at turn end
    /// against servers that send both.
    pub committed_tools: bool,
}

impl FoldState {
    /// Commit what the folder still holds open (end of the current tool
    /// group), then reset it to the naming prefix.
    pub fn flush_tools(&mut self, vm: &mut SessionViewModel) {
        if !self.open_group.is_empty() {
            let mut log = Vec::with_capacity(self.prefix.len() + self.open_group.len());
            log.extend_from_slice(&self.prefix);
            log.extend_from_slice(&self.open_group);
            // The prefix's own rows committed when it was folded live;
            // only the group's held summary is new.
            let entries = replay(&log);
            let skip = replay(&self.prefix).len().min(entries.len());
            for entry in &entries[skip..] {
                if matches!(entry, TranscriptEntry::ToolSummary { .. }) {
                    self.committed_tools = true;
                }
                commit(vm, entry.clone());
            }
        }
        self.reset_to_prefix();
    }

    /// Reset the folder to the naming prefix: the group's rows are all
    /// committed by now, so later events fold from the prefix forward
    /// (a fresh tool run, named by the prefix's `tool_calls`).
    fn reset_to_prefix(&mut self) {
        self.folder.reset();
        for event in &self.prefix {
            // The prefix's own rows committed when it was folded live;
            // only the folder's naming state (call ids) matters here.
            let _ = self.folder.push(event);
        }
        self.open_group.clear();
    }

    /// Fold one live `SessionEvent` into committed rows.
    pub fn fold_event(&mut self, vm: &mut SessionViewModel, event: SessionEvent) {
        // A persisted task echo settles its pending steer (the server
        // injects a steer as a `Task` event, same text): the pending echo
        // gives way to the committed row below, in transcript order.
        if let SessionEvent::Task { text } = &event {
            if let Some(index) = vm.pending_steers.iter().position(|steer| steer == text) {
                vm.pending_steers.remove(index);
            }
        }
        // A persisted assistant message supersedes the streamed buffer
        // (the replayed row carries the authoritative text).
        if let SessionEvent::Message { message, .. } = &event {
            if message.role == Role::Assistant {
                vm.streaming_text.clear();
                vm.streaming_reasoning.clear();
            }
        }
        // The single incremental push: the folder returns exactly the
        // rows this event lands (a boundary event flushes the open tool
        // group first, a ToolResult joins it). A trailing ToolSummary
        // stays open inside the folder until the next boundary — the
        // old `held` bookkeeping, done by the shared fold.
        for entry in self.folder.push(&event) {
            if matches!(entry, TranscriptEntry::ToolSummary { .. }) {
                self.committed_tools = true;
            }
            commit(vm, entry.clone());
        }
        // Usage accounting: persisted Usage events are ground truth.
        if let SessionEvent::Usage { usage, .. } = event {
            vm.cumulative_usage.input_tokens += usage.input_tokens;
            vm.cumulative_usage.output_tokens += usage.output_tokens;
            vm.cumulative_usage.cached_input_tokens += usage.cached_input_tokens;
        }
        // Track the open group for the turn-end flush (`flush_tools`):
        // the naming prefix rolls forward on every persisted assistant
        // message, and ToolResults accumulate until the next boundary
        // clears them.
        match &event {
            SessionEvent::ToolResult { .. } => self.open_group.push(event),
            SessionEvent::Message { message, .. } if message.role == Role::Assistant => {
                self.prefix = vec![event];
                self.open_group.clear();
            }
            _ => self.open_group.clear(),
        }
    }

    /// Reset everything from a snapshot's live half.
    pub fn seed_from_live(&mut self, vm: &mut SessionViewModel, live: &LiveState) {
        self.tracker = ActivityTracker {
            turn_tools: live.turn_tools.clone(),
            subagents: live
                .subagents
                .iter()
                .map(|row| SwarmRow {
                    id: row.id.clone(),
                    label: row.label.clone(),
                    turns: row.turns,
                    tokens: row.tokens,
                    last_tool: row.last_tool.clone(),
                    done: row.done,
                    started_ms: None,
                    state: row.state,
                })
                .collect(),
        };
        vm.turn_tools = self.tracker.turn_tools.clone();
        vm.subagents = self.tracker.subagents.clone();
        self.folder.reset();
        self.prefix.clear();
        self.open_group.clear();
        self.committed_tools = false;
    }

    /// Fold one live `Tool` frame through the canonical tracker.
    pub fn fold_tool(&mut self, vm: &mut SessionViewModel, activity: &ToolActivity) {
        self.tracker.on_tool(activity);
        vm.turn_tools = self.tracker.turn_tools.clone();
        vm.subagents = self.tracker.subagents.clone();
    }
}

/// Append a committed row to history and the drain queue, rendering the
/// shared strings ONCE here (a poll clones the finished rows; it never
/// re-runs `entry_lines`). Both stores drop their oldest rows past
/// [`HISTORY_CAP`].
pub(crate) fn commit(vm: &mut SessionViewModel, entry: TranscriptEntry) {
    vm.history.push(rendered_row(&entry));
    cap_rows(&mut vm.history);
    vm.new_entries.push(entry);
    cap_rows(&mut vm.new_entries);
}

/// Fold a snapshot: `replay(events)` replaces the committed history in
/// full, the live half seeds the in-flight view, and the naming prefix
/// rolls forward from the log. Usage resets to the snapshot's cumulative
/// value so a reconnect never double-counts.
pub(crate) fn fold_snapshot(
    vm: &mut SessionViewModel,
    fold: &mut FoldState,
    session_id: Uuid,
    last_seq: u64,
    events: &[SessionEvent],
    live: &LiveState,
) {
    vm.session_id = Some(session_id);
    vm.last_seq = vm.last_seq.max(last_seq);
    // Snapshot replay becomes the committed history: rendered once here,
    // never again per poll. (The `new_entries` batch carries the typed
    // entries for incremental shells.)
    let entries = replay(events);
    let rows: Vec<RenderedRow> = entries.iter().map(rendered_row).collect();
    vm.history = rows;
    vm.new_entries = entries;
    cap_rows(&mut vm.history);
    cap_rows(&mut vm.new_entries);
    fold.prefix = events
        .iter()
        .rev()
        .find(|event| {
            matches!(
                event,
                SessionEvent::Message {
                    message,
                    ..
                } if message.role == Role::Assistant
            )
        })
        .cloned()
        .into_iter()
        .collect();
    fold.reset_to_prefix();
    vm.streaming_text = live.streaming_text.clone();
    // Reasoning is opt-in (invariant 5): a snapshot's live reasoning
    // buffer only seeds when the user asked for it, same as the fold's
    // delta gate. `streaming_text` always seeds — text streaming is on
    // by definition.
    vm.streaming_reasoning = if vm.show_reasoning {
        live.streaming_reasoning.clone()
    } else {
        String::new()
    };
    vm.running = live.running;
    vm.model = live.model.clone();
    vm.provider = live.provider.clone();
    vm.goal = live.goal.clone();
    // H1-H9b: queue, steer echoes, phase, plan, context and rules are the
    // snapshot's whole truth (a reattach never keeps stale values).
    vm.queued = live.queued.clone();
    vm.pending_steers.clear();
    vm.phase = live.phase.unwrap_or_default();
    vm.effort = live.effort;
    // The Snapshot carries only the pending flag, so the plan's lines stay
    // empty until a live `PlanProposed` arrives.
    vm.plan_pending = live
        .plan_pending
        .then(|| PendingPlanView::new(&[] as &[PlanItem]));
    vm.context = live.context;
    vm.rules = live.rules.clone();
    // C5: the server's real approval mode and sandbox (an absent field
    // keeps the client's default), and the server's allow/deny lists, so
    // the tool editor starts from what the server holds.
    if let Some(mode) = live.approval_mode {
        vm.approval_mode = Some(mode);
    }
    if let Some(policy) = live.sandbox {
        vm.sandbox = policy;
    }
    vm.allow_tools = live.allow.clone();
    vm.deny_tools = live.deny.clone();
    vm.cumulative_usage = live.cumulative_usage;
    // The snapshot is the whole truth for approvals: its one pending
    // request (if any) replaces whatever was queued before the reattach.
    vm.pending_approvals.clear();
    if let Some(PendingApproval {
        request_id,
        call,
        agent,
    }) = &live.pending_approval
    {
        enqueue_approval(vm, request_id.clone(), call, agent.as_deref());
    }
    // Counters are re-derived from the log, not accumulated onto the old
    // values (a reattach would otherwise double-count).
    let (turns, tool_calls) = count_log(events);
    vm.turns_completed = turns;
    vm.tool_calls_total = tool_calls;
    fold.seed_from_live(vm, live);
}

/// Session counters from a persisted log, the same units the live
/// `TurnDone` summary reports: one model turn per per-turn `TurnTrace`
/// record (verifier records and the terminal stop record are not turns),
/// and one tool call per `ToolResult` (the engine appends one per executed
/// call).
pub(crate) fn count_log(events: &[SessionEvent]) -> (u32, u32) {
    let mut turns = 0u32;
    let mut tool_calls = 0u32;
    for event in events {
        match event {
            SessionEvent::TurnTrace {
                verifier: None,
                failure_label: None,
                ..
            } => turns += 1,
            SessionEvent::ToolResult { .. } => tool_calls += 1,
            _ => {}
        }
    }
    (turns, tool_calls)
}

/// Queue an approval request. A repeat of an id already queued replaces
/// that entry in place (no double prompt for one request).
pub(crate) fn enqueue_approval(
    vm: &mut SessionViewModel,
    request_id: String,
    call: &ToolCall,
    agent: Option<&str>,
) {
    let view = PendingApprovalView::for_agent(request_id, call, agent);
    match vm
        .pending_approvals
        .iter()
        .position(|p| p.request_id == view.request_id)
    {
        Some(index) => vm.pending_approvals[index] = view,
        None => vm.pending_approvals.push_back(view),
    }
}

/// Remove one request from the queue wherever it sits. Unknown ids are a
/// no-op (the server echo of an answer the client already removed).
pub(crate) fn resolve_approval(vm: &mut SessionViewModel, request_id: &str) {
    vm.pending_approvals.retain(|p| p.request_id != request_id);
}

/// `TurnDone`: held tool rows commit, the streaming buffer commits as the
/// assistant turn (or the summary answer when the server never streamed),
/// the swarm block commits, and the live view goes idle.
pub(crate) fn fold_turn_done(
    vm: &mut SessionViewModel,
    fold: &mut FoldState,
    summary: &WireRunSummary,
) {
    fold.flush_tools(vm);
    // TUI parity: the turn's live tool strip lands as ONE grouped summary
    // line, committed before the answer — unless the Event path already
    // committed one.
    if !fold.committed_tools && !fold.tracker.turn_tools.is_empty() {
        commit(
            vm,
            TranscriptEntry::ToolSummary {
                line: modron_view::tool_summary_line(&fold.tracker.turn_tools),
                events: fold.tracker.turn_tools.clone(),
            },
        );
    }
    fold.committed_tools = false;
    let mut text = std::mem::take(&mut vm.streaming_text);
    if text.is_empty() {
        text = summary.answer.clone();
    }
    // Servers that persist the assistant message AND send TurnDone would
    // double-commit; the identical last assistant row suppresses that.
    let last_assistant = vm.history.iter().rev().find_map(|row| match &row.entry {
        TranscriptEntry::Assistant { text } => Some(text.as_str()),
        _ => None,
    });
    let already_committed = last_assistant == Some(text.as_str());
    if !text.is_empty() && !already_committed {
        commit(vm, TranscriptEntry::Assistant { text });
    }
    vm.streaming_reasoning.clear();
    let lines = fold.tracker.take_swarm_summary();
    if !lines.is_empty() {
        commit(vm, TranscriptEntry::Swarm { lines });
    }
    vm.turn_tools.clear();
    vm.subagents.clear();
    vm.pending_steers.clear();
    vm.running = false;
    vm.pending_approvals.clear();
    vm.turns_completed += summary.turns;
    vm.tool_calls_total += summary.tool_calls;
}

/// `TurnFailed`/`TurnAborted`: persisted rows commit (they already
/// happened), streaming buffers clear uncommitted (TUI abort parity),
/// and the live view goes idle. `error` surfaces as `last_error`.
pub(crate) fn fold_turn_unfinished(
    vm: &mut SessionViewModel,
    fold: &mut FoldState,
    error: Option<String>,
) {
    let _ = fold.tracker.take_swarm_summary();
    fold.flush_tools(vm);
    // TUI interrupt parity: the partial strip commits as a summary line.
    if !fold.committed_tools && !fold.tracker.turn_tools.is_empty() {
        commit(
            vm,
            TranscriptEntry::ToolSummary {
                line: modron_view::tool_summary_line(&fold.tracker.turn_tools),
                events: fold.tracker.turn_tools.clone(),
            },
        );
    }
    fold.committed_tools = false;
    vm.streaming_text.clear();
    vm.streaming_reasoning.clear();
    vm.turn_tools.clear();
    vm.subagents.clear();
    vm.pending_steers.clear();
    vm.running = false;
    vm.pending_approvals.clear();
    if let Some(error) = error {
        vm.last_error = Some(error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use modron_types::{Message, ToolOutcome};

    fn assistant_with_calls(content: &str, call_id: &str) -> SessionEvent {
        let mut message = Message::assistant(content);
        message.tool_calls = vec![ToolCall {
            id: call_id.into(),
            name: "read".into(),
            arguments: serde_json::json!({"path": "a.rs"}),
        }];
        SessionEvent::Message {
            parent_id: None,
            message,
        }
    }

    #[test]
    fn pending_approval_summary_is_the_shared_string() {
        let long = "p".repeat(200);
        let call = ToolCall {
            id: "c".into(),
            name: "bash".into(),
            arguments: serde_json::json!({ "command": long.clone() }),
        };
        let view = PendingApprovalView::new("r".into(), &call);
        assert_eq!(
            view.args_summary,
            modron_view::approval_summary(
                "bash",
                &call.arguments,
                modron_view::APPROVAL_SUMMARY_MAX
            )
        );
        assert!(
            view.args_summary.ends_with(" [+120 chars]"),
            "{}",
            view.args_summary
        );
        // The sheet's expandable block carries the whole call, untruncated.
        assert!(view.arguments_text.contains(&long));

        // Bidi overrides cannot reorder the full-call text either.
        let call = ToolCall {
            id: "c".into(),
            name: "write".into(),
            arguments: serde_json::json!({ "path": "a\u{202e}b" }),
        };
        let view = PendingApprovalView::new("r".into(), &call);
        assert_eq!(view.args_summary, r#"a?b"#);
        assert_eq!(view.arguments_text, r#"{"path":"a?b"}"#);
    }

    /// H3b: a child's ask reads `[agent] <summary>`; the run's own call
    /// keeps the shared summary unchanged.
    #[test]
    fn child_approval_summary_is_prefixed_with_its_agent() {
        let call = ToolCall {
            id: "c".into(),
            name: "bash".into(),
            arguments: serde_json::json!({ "command": "ls" }),
        };
        let own = PendingApprovalView::new("r".into(), &call);
        assert_eq!(
            own.args_summary,
            PendingApprovalView::for_agent("r".into(), &call, None).args_summary
        );
        let child = PendingApprovalView::for_agent("r".into(), &call, Some("delegate"));
        assert_eq!(
            child.args_summary,
            format!("[delegate] {}", own.args_summary)
        );
        assert_eq!(child.tool, "bash");
    }

    #[test]
    fn decision_labels_match_the_shared_words() {
        assert_eq!(DECISION_LABELS[0], (Decision::AllowOnce, "allow once"));
        assert_eq!(DECISION_LABELS[1], (Decision::AllowAll, "allow all"));
        assert_eq!(DECISION_LABELS[2], (Decision::Deny, "deny"));
        for (i, decision) in Decision::ALL.into_iter().enumerate() {
            assert_eq!(DECISION_LABELS[i].0, decision);
            assert_eq!(DECISION_LABELS[i].1, decision.label());
        }
        assert_eq!(approval_mode_label(None), "ask");
        assert_eq!(
            approval_mode_label(Some(ApprovalModeWire::ApproveAll)),
            "yolo"
        );
    }

    #[test]
    fn snapshot_carries_mode_sandbox_and_lists() {
        // C5: attach to a yolo session: the label is yolo, the sandbox and
        // the allow/deny copy come from the server.
        let live = LiveState {
            approval_mode: Some(ApprovalModeWire::ApproveAll),
            sandbox: Some(modron_types::SandboxPolicy::Bubblewrap),
            allow: vec!["diagnostics".into()],
            deny: vec!["bash".into()],
            ..LiveState::default()
        };
        let mut vm = SessionViewModel::default();
        let mut fold = FoldState::default();
        fold_snapshot(&mut vm, &mut fold, Uuid::nil(), 1, &[], &live);
        let snap = ViewModelSnapshot::from_vm(&vm);
        assert_eq!(snap.approval_mode_label, "yolo");
        assert_eq!(snap.sandbox, "bwrap");
        assert_eq!(snap.allow_tools, vec!["diagnostics".to_string()]);
        assert_eq!(snap.deny_tools, vec!["bash".to_string()]);
        assert!(
            snap.status_line.ends_with(" · yolo"),
            "{}",
            snap.status_line
        );

        // An old snapshot without the fields: the label stays at its
        // default (ask) and the lists are empty.
        let mut fresh = SessionViewModel::default();
        fold_snapshot(
            &mut fresh,
            &mut fold,
            Uuid::nil(),
            1,
            &[],
            &LiveState::default(),
        );
        let snap = ViewModelSnapshot::from_vm(&fresh);
        assert_eq!(snap.approval_mode_label, "ask");
        assert!(snap.allow_tools.is_empty() && snap.deny_tools.is_empty());
    }

    #[test]
    fn snapshot_seeds_reasoning_only_when_opted_in() {
        // Invariant 5: reasoning streams only when opted in — also at the
        // snapshot boundary (a reconnect must not surface a live
        // reasoning buffer the user never asked to see).
        let session = Uuid::nil();
        let live = LiveState {
            streaming_reasoning: "thinking".into(),
            ..LiveState::default()
        };
        let mut vm = SessionViewModel::default();
        let mut fold = FoldState::default();
        fold_snapshot(&mut vm, &mut fold, session, 1, &[], &live);
        assert!(!vm.show_reasoning, "show_reasoning defaults off");
        assert_eq!(vm.streaming_reasoning, "", "not seeded while off");
        let snap = ViewModelSnapshot::from_vm(&vm);
        assert!(!snap.show_reasoning);
        assert_eq!(snap.streaming_reasoning, "");
        // Opted in: the snapshot's live buffer seeds (mid-run attach).
        vm.show_reasoning = true;
        fold_snapshot(&mut vm, &mut fold, session, 1, &[], &live);
        assert_eq!(vm.streaming_reasoning, "thinking");
        assert!(ViewModelSnapshot::from_vm(&vm).show_reasoning);
    }

    #[test]
    fn event_stream_groups_tools_like_a_whole_log_replay() {
        // Folding event-by-event must produce the same rows as replaying
        // the entire log at once (the parity contract).
        let events = vec![
            SessionEvent::Task {
                text: "read a.rs".into(),
            },
            assistant_with_calls("looking at the file", "call-1"),
            SessionEvent::ToolResult {
                call_id: "call-1".into(),
                outcome: ToolOutcome::ok("contents"),
            },
            SessionEvent::ToolResult {
                call_id: "call-1".into(),
                outcome: ToolOutcome::err("nope"),
            },
            SessionEvent::Message {
                parent_id: None,
                message: Message::assistant("done"),
            },
            SessionEvent::Usage {
                model: "m".into(),
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    cached_input_tokens: 4,
                },
            },
        ];
        let mut vm = SessionViewModel::default();
        let mut fold = FoldState::default();
        for event in &events {
            fold.fold_event(&mut vm, event.clone());
        }
        fold.flush_tools(&mut vm);
        let incremental: Vec<TranscriptEntry> =
            vm.history.iter().map(|row| row.entry.clone()).collect();
        let whole = replay(&events);
        assert_eq!(incremental, whole, "incremental fold == one-shot replay");
    }

    #[test]
    fn turn_end_flush_commits_an_open_tool_group() {
        // A log ending in a run of ToolResults: replay commits the group
        // at end-of-log; the live fold holds it open inside the folder
        // and the turn boundary (`flush_tools`) commits the same row.
        let events = vec![
            SessionEvent::Task {
                text: "edit a.rs".into(),
            },
            assistant_with_calls("making the change", "call-1"),
            SessionEvent::ToolResult {
                call_id: "call-1".into(),
                outcome: ToolOutcome::ok("written"),
            },
            SessionEvent::ToolResult {
                call_id: "call-1".into(),
                outcome: ToolOutcome::err("boom"),
            },
        ];
        let mut vm = SessionViewModel::default();
        let mut fold = FoldState::default();
        for event in &events {
            fold.fold_event(&mut vm, event.clone());
        }
        assert!(
            vm.history
                .iter()
                .all(|row| !matches!(row.entry, TranscriptEntry::ToolSummary { .. })),
            "the trailing group stays open before the flush"
        );
        fold.flush_tools(&mut vm);
        let incremental: Vec<TranscriptEntry> =
            vm.history.iter().map(|row| row.entry.clone()).collect();
        assert_eq!(incremental, replay(&events));
        assert!(fold.committed_tools, "the held summary marks the commit");
        // The next turn folds from the prefix forward: another result of
        // the same call lands in a FRESH group (one summary), and the
        // committed_tools guard resets with it.
        fold.committed_tools = false;
        fold.fold_event(
            &mut vm,
            SessionEvent::ToolResult {
                call_id: "call-1".into(),
                outcome: ToolOutcome::ok("more"),
            },
        );
        fold.flush_tools(&mut vm);
        let summaries = vm
            .history
            .iter()
            .filter(|row| matches!(row.entry, TranscriptEntry::ToolSummary { .. }))
            .count();
        assert_eq!(summaries, 2, "each turn's group lands as one summary");
    }

    #[test]
    fn fold_event_accumulates_usage() {
        let mut vm = SessionViewModel::default();
        let mut fold = FoldState::default();
        fold.fold_event(
            &mut vm,
            SessionEvent::Usage {
                model: "m".into(),
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 2,
                    cached_input_tokens: 0,
                },
            },
        );
        fold.fold_event(
            &mut vm,
            SessionEvent::Usage {
                model: "m".into(),
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    cached_input_tokens: 3,
                },
            },
        );
        assert_eq!(vm.cumulative_usage.total(), 14);
    }

    #[test]
    fn snapshot_status_line_matches_tui_words() {
        let vm = SessionViewModel {
            model: "gpt-5.6-sol".into(),
            cumulative_usage: Usage {
                input_tokens: 300,
                output_tokens: 100,
                cached_input_tokens: 100,
            },
            approval_mode: Some(ApprovalModeWire::ApproveAll),
            ..SessionViewModel::default()
        };
        let snap = ViewModelSnapshot::from_vm(&vm);
        assert_eq!(snap.status_line, "gpt-5.6-sol · 400 tok · cache 33% · yolo");
        assert_eq!(snap.running_label, "idle");
    }

    #[test]
    fn entry_lines_render_shared_copy() {
        let rows = entry_lines(&TranscriptEntry::Task {
            text: "fix it".into(),
        });
        assert_eq!(rows, vec!["❯ fix it"]);
        let rows = entry_lines(&TranscriptEntry::ToolSummary {
            line: "⏺ 1 tool · read".into(),
            events: vec![],
        });
        assert_eq!(rows, vec!["⏺ 1 tool · read"]);
        let rows = entry_lines(&TranscriptEntry::Usage {
            model: "m".into(),
            usage: Usage {
                input_tokens: 75,
                output_tokens: 25,
                cached_input_tokens: 25,
            },
        });
        assert_eq!(rows, vec!["100 tok · cache 33%"]);
    }

    fn steer_vm(texts: &[&str]) -> SessionViewModel {
        SessionViewModel {
            running: true,
            pending_steers: texts.iter().map(|t| t.to_string()).collect(),
            ..SessionViewModel::default()
        }
    }

    #[test]
    fn steer_echo_is_removed_by_its_persisted_task_event() {
        let mut vm = steer_vm(&["also check B", "and C"]);
        let mut fold = FoldState::default();
        fold.fold_event(
            &mut vm,
            SessionEvent::Task {
                text: "also check B".into(),
            },
        );
        assert_eq!(vm.pending_steers, vec!["and C".to_string()]);
        // The committed task row lands in history as usual.
        assert!(vm
            .history
            .iter()
            .any(|row| row.lines == vec!["❯ also check B".to_string()]));
        // An unrelated task leaves the pending echo alone.
        fold.fold_event(
            &mut vm,
            SessionEvent::Task {
                text: "something else".into(),
            },
        );
        assert_eq!(vm.pending_steers, vec!["and C".to_string()]);
    }

    #[test]
    fn steer_echoes_clear_on_abort_failure_and_turn_done() {
        let mut fold = FoldState::default();
        let mut vm = steer_vm(&["one"]);
        fold_turn_unfinished(&mut vm, &mut fold, None);
        assert!(vm.pending_steers.is_empty(), "abort clears steers");

        let mut vm = steer_vm(&["two"]);
        fold_turn_unfinished(&mut vm, &mut fold, Some("boom".into()));
        assert!(vm.pending_steers.is_empty(), "failure clears steers");

        let mut vm = steer_vm(&["three"]);
        let summary = WireRunSummary {
            session_id: Uuid::nil(),
            answer: String::new(),
            turns: 0,
            tool_calls: 0,
            usage: Usage::default(),
            models_used: Vec::new(),
        };
        fold_turn_done(&mut vm, &mut fold, &summary);
        assert!(vm.pending_steers.is_empty(), "turn done clears steers");
    }

    #[test]
    fn snapshot_resets_queue_steers_phase_context_and_rules() {
        let session = Uuid::new_v4();
        let mut vm = SessionViewModel {
            queued: vec!["stale".into()],
            pending_steers: vec!["stale steer".into()],
            plan_pending: Some(PendingPlanView::new(&[])),
            phase: Phase::Plan,
            context: Some(ContextGaugeWire { used: 1, window: 2 }),
            ..SessionViewModel::default()
        };
        let mut fold = FoldState::default();
        let live = LiveState {
            queued: vec!["next".into()],
            phase: Some(Phase::Build),
            context: Some(ContextGaugeWire {
                used: 74_000,
                window: 180_000,
            }),
            rules: vec![RuleWire {
                effect: "deny".into(),
                spec: "bash(rm -rf*)".into(),
            }],
            ..LiveState::default()
        };
        fold_snapshot(&mut vm, &mut fold, session, 3, &[], &live);
        assert_eq!(vm.queued, vec!["next".to_string()]);
        assert!(vm.pending_steers.is_empty());
        assert!(vm.plan_pending.is_none(), "snapshot says not pending");
        assert_eq!(vm.phase, Phase::Build);
        assert_eq!(
            vm.context,
            Some(ContextGaugeWire {
                used: 74_000,
                window: 180_000
            })
        );
        assert_eq!(vm.rules.len(), 1);

        // A snapshot that says a plan awaits keeps the block with no lines.
        let live = LiveState {
            plan_pending: true,
            ..LiveState::default()
        };
        fold_snapshot(&mut vm, &mut fold, session, 4, &[], &live);
        let plan = vm.plan_pending.clone().expect("pending plan on attach");
        assert!(plan.lines.is_empty(), "attach carries no items");
        assert_eq!(plan.options[0].label, "approve");
        assert_eq!(plan.options[1].label, "revise");
        // Absent context and phase reset to the defaults.
        assert_eq!(vm.phase, Phase::Build);
        assert!(vm.context.is_none());
    }

    #[test]
    fn plan_block_renders_items_and_the_shared_option_labels() {
        let items = vec![PlanItem {
            text: "Edit the parser".into(),
            done_criteria: "parser tests pass".into(),
            status: "pending".into(),
        }];
        let plan = PendingPlanView::new(&items);
        assert_eq!(plan.lines, vec!["○ Edit the parser - parser tests pass"]);
        assert_eq!(plan.options[0].decision, PlanDecision::Approve);
        assert_eq!(plan.options[0].label, "approve");
        assert_eq!(plan.options[1].decision, PlanDecision::Revise);
        assert_eq!(plan.options[1].label, "revise");
        assert_eq!(
            PLAN_DECISION_LABELS.map(|(_, label)| label),
            ["approve", "revise"]
        );
    }

    #[test]
    fn effort_shows_next_to_the_model_only_when_set() {
        let mut vm = SessionViewModel {
            model: "m".into(),
            ..SessionViewModel::default()
        };
        let snap = ViewModelSnapshot::from_vm(&vm);
        assert_eq!(snap.status_line, "m · 0 tok · ask");
        assert_eq!(snap.effort_label, "");

        vm.effort = Some(Effort::Xhigh);
        let snap = ViewModelSnapshot::from_vm(&vm);
        assert_eq!(snap.status_line, "m · effort xhigh · 0 tok · ask");
        assert_eq!(snap.effort_label, "xhigh");
        assert_eq!(effort_label(None), "");
    }

    #[test]
    fn context_label_formats_the_meter_and_hides_when_unknown() {
        assert_eq!(context_label(None), "");
        // A zero window has no percentage: the meter stays hidden.
        assert_eq!(
            context_label(Some(ContextGaugeWire { used: 5, window: 0 })),
            ""
        );
        assert_eq!(
            context_label(Some(ContextGaugeWire {
                used: 74_000,
                window: 180_000
            })),
            "ctx 41% (74k/180k)"
        );
        assert_eq!(
            context_label(Some(ContextGaugeWire {
                used: 0,
                window: 200
            })),
            "ctx 0% (0/200)"
        );
    }

    #[test]
    fn status_line_shows_plan_phase_and_context_only_when_relevant() {
        let mut vm = SessionViewModel {
            model: "m".into(),
            context: Some(ContextGaugeWire {
                used: 74_000,
                window: 180_000,
            }),
            ..SessionViewModel::default()
        };
        // Build: no phase word; the context meter still shows.
        let snap = ViewModelSnapshot::from_vm(&vm);
        assert_eq!(snap.status_line, "m · 0 tok · ask · ctx 41% (74k/180k)");
        assert_eq!(snap.phase_label, "build");
        assert_eq!(snap.context_label, "ctx 41% (74k/180k)");

        vm.phase = Phase::Plan;
        let snap = ViewModelSnapshot::from_vm(&vm);
        assert_eq!(
            snap.status_line,
            "m · 0 tok · ask · plan · ctx 41% (74k/180k)"
        );
        assert_eq!(snap.phase_label, "plan");
    }

    #[test]
    fn rules_and_queue_project_into_the_snapshot() {
        let vm = SessionViewModel {
            queued: vec!["a".into(), "b".into()],
            rules: vec![RuleWire {
                effect: "allow".into(),
                spec: "read".into(),
            }],
            ..SessionViewModel::default()
        };
        let snap = ViewModelSnapshot::from_vm(&vm);
        assert_eq!(snap.queued, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(snap.rules, vec!["allow read".to_string()]);
        assert!(snap.context_label.is_empty());
        assert!(snap.plan_pending.is_none());
    }
}

#[cfg(test)]
mod copy_tests {
    use super::*;
    use modron_types::SandboxPolicy;

    fn meta(id: &str, title: &str, count: u32, state: Option<&str>) -> SessionMetaWire {
        SessionMetaWire {
            id: Uuid::parse_str(id).expect("uuid"),
            title: title.into(),
            created_at: String::new(),
            updated_at: String::new(),
            message_count: count,
            task_branch: None,
            task_state: state.map(String::from),
            working_dir: None,
        }
    }

    const ID_A: &str = "abcdef01-2345-6789-abcd-ef0123456789";
    const ID_B: &str = "11111111-2222-3333-4444-555555555555";

    #[test]
    fn session_row_shows_title_state_and_count() {
        let m = meta(ID_A, "router fix", 3, None);
        assert_eq!(
            session_row(&m, false),
            "router fix \u{b7} idle \u{b7} 3 messages"
        );
        assert_eq!(
            session_row(&m, true),
            "router fix \u{b7} running \u{b7} 3 messages"
        );
    }

    #[test]
    fn an_untitled_session_shows_its_short_id() {
        let m = meta(ID_A, "  ", 0, None);
        assert_eq!(
            session_row(&m, false),
            "abcdef01 \u{b7} idle \u{b7} 0 messages"
        );
    }

    #[test]
    fn approval_cycle_is_ask_yolo_deny() {
        assert_eq!(
            approval_mode_next(Some(ApprovalModeWire::Ask)),
            ApprovalModeWire::ApproveAll
        );
        assert_eq!(
            approval_mode_next(Some(ApprovalModeWire::ApproveAll)),
            ApprovalModeWire::Deny
        );
        assert_eq!(
            approval_mode_next(Some(ApprovalModeWire::Deny)),
            ApprovalModeWire::Ask
        );
        assert_eq!(approval_mode_next(None), ApprovalModeWire::ApproveAll);
    }

    #[test]
    fn effort_cycle_runs_unset_through_max_and_back() {
        let mut effort = None;
        let mut seen = Vec::new();
        for _ in 0..7 {
            effort = effort_next(effort);
            seen.push(effort.map(|e| e.label()).unwrap_or(EFFORT_UNSET_LABEL));
        }
        assert_eq!(
            seen,
            ["low", "medium", "high", "xhigh", "max", "unset", "low"]
        );
    }

    #[test]
    fn phase_and_sandbox_cycles() {
        assert_eq!(phase_next(Phase::Build), Phase::Plan);
        assert_eq!(phase_next(Phase::Plan), Phase::Build);
        let s = SandboxPolicy::Off;
        assert_eq!(sandbox_next(s), SandboxPolicy::Seatbelt);
        assert_eq!(sandbox_next(sandbox_next(s)), SandboxPolicy::Bubblewrap);
        assert_eq!(
            sandbox_next(sandbox_next(sandbox_next(s))),
            SandboxPolicy::Off
        );
    }

    #[test]
    fn snapshot_rows_follow_the_session_list_and_running_state() {
        let a = Uuid::parse_str(ID_A).expect("uuid");
        let vm = SessionViewModel {
            session_id: Some(a),
            running: true,
            sessions: vec![
                meta(ID_A, "attached", 2, None),
                meta(ID_B, "other", 5, None),
            ],
            ..Default::default()
        };
        let snap = ViewModelSnapshot::from_vm(&vm);
        assert_eq!(snap.session_rows.len(), snap.sessions.len());
        assert_eq!(
            snap.session_rows[0],
            "attached \u{b7} running \u{b7} 2 messages"
        );
        assert_eq!(snap.session_rows[1], "other \u{b7} idle \u{b7} 5 messages");
    }

    #[test]
    fn a_task_marked_running_on_the_server_shows_running() {
        let vm = SessionViewModel {
            sessions: vec![meta(ID_B, "task", 1, Some("running"))],
            ..Default::default()
        };
        let snap = ViewModelSnapshot::from_vm(&vm);
        assert_eq!(
            snap.session_rows,
            vec!["task \u{b7} running \u{b7} 1 messages".to_string()]
        );
    }
}
