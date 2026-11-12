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
//!   `streaming_reasoning`. Buffers commit to history at `TurnDone` and
//!   clear (uncommitted) on abort/failure — TUI streaming parity.
//! - `Event` frames append committed rows by folding through
//!   [`modron_view::replay`] (see `FoldState` for the incremental
//!   grouping bookkeeping).
//! - `Tool` frames feed [`modron_view::ActivityTracker`] (the live strip).
//! - `ApprovalRequested` blocks on a decision; `ApprovalResolved` clears
//!   it. Config changes are label updates for future turns only.
//!
//! Usage accounting: cumulative usage grows from persisted `Usage` events
//! (`usage_totals` arithmetic) and resets to the snapshot's live value on
//! attach, so a reconnect never double-counts. `TurnDone` summaries do
//! not add usage (the server's `Usage` events already covered the turn).

use modron_protocol::{
    ApprovalModeWire, Decision, LiveState, PendingApproval, SessionMetaWire, WireRunSummary,
};
use modron_types::{Role, SessionEvent, ToolActivity, ToolCall, Usage};
use modron_view::{replay, ActivityTracker, SwarmRow, ToolEvent, TranscriptEntry};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Parity constants — the exact shared words (AGENTS.md UI/UX parity contract)
// ---------------------------------------------------------------------------

/// Decision labels for the approval sheet, in protocol order. Same words
/// as the TUI's approval strip (`y = allow once · a = allow for this run ·
/// n/Esc = deny`, modron-tui run.rs); keys differ per the divergence table.
pub const DECISION_LABELS: [(Decision, &str); 3] = [
    (Decision::AllowOnce, "allow once"),
    (Decision::AllowAll, "allow for this run"),
    (Decision::Deny, "deny"),
];

/// The approval mode label for the status line (`ask`/`yolo`/`deny`,
/// default `ask`), matching the TUI status strip exactly.
pub fn approval_mode_label(mode: Option<ApprovalModeWire>) -> &'static str {
    match mode {
        Some(ApprovalModeWire::ApproveAll) => "yolo",
        Some(ApprovalModeWire::Ask) | None => "ask",
        Some(ApprovalModeWire::Deny) => "deny",
    }
}

/// The sandbox label for the settings views, using the desktop's words
/// (`off` / `seatbelt` / `bwrap`).
pub fn sandbox_label(policy: modron_types::SandboxPolicy) -> &'static str {
    match policy {
        modron_types::SandboxPolicy::Off => "off",
        modron_types::SandboxPolicy::Seatbelt => "seatbelt",
        modron_types::SandboxPolicy::Bubblewrap => "bwrap",
    }
}

/// Connection-state words for the mobile header.
pub fn connection_label(state: &ConnectionState) -> &'static str {
    match state {
        ConnectionState::Connected => "connected",
        ConnectionState::Connecting => "connecting",
        ConnectionState::Disconnected => "disconnected",
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
            // status line on mobile instead.
            let all = usage.input_tokens + usage.cached_input_tokens;
            let cache = if all > 0 {
                format!(
                    "cache {:.0}%",
                    100.0 * usage.cached_input_tokens as f64 / all as f64
                )
            } else {
                "cache —".to_string()
            };
            vec![format!("{} tok · {}", usage.total(), cache)]
        }
        TranscriptEntry::Compaction {
            summary,
            summarized_events,
        } => vec![format!("compacted {summarized_events} events: {summary}")],
        TranscriptEntry::GenerativeUi { ui } => modron_view::render_ascii(ui),
        TranscriptEntry::Plan { items } => modron_view::plan_lines(items),
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
    /// One-line arguments summary (truncated JSON).
    pub args_summary: String,
    /// Full parsed arguments for the detail sheet.
    pub arguments: serde_json::Value,
    /// The three decisions with their shared labels, protocol order.
    pub options: Vec<DecisionOption>,
}

/// One selectable decision on the approval sheet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionOption {
    pub decision: Decision,
    /// Shared label (`allow once` / `allow for this run` / `deny`).
    pub label: String,
}

/// Arguments summary cap (chars); keeps the sheet to one line.
const ARGS_SUMMARY_MAX: usize = 120;

impl PendingApprovalView {
    pub fn new(request_id: String, call: &ToolCall) -> Self {
        let full = serde_json::to_string(&call.arguments).unwrap_or_default();
        let mut args_summary: String = full.chars().take(ARGS_SUMMARY_MAX).collect();
        if full.chars().count() > ARGS_SUMMARY_MAX {
            args_summary.push('…');
        }
        Self {
            request_id,
            tool: call.name.clone(),
            args_summary,
            arguments: call.arguments.clone(),
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
    /// Live grouped tool strip (`ToolEvent::summary()` rows).
    pub turn_tools: Vec<ToolEvent>,
    /// Live swarm rows.
    pub subagents: Vec<SwarmRow>,
    pub pending_approval: Option<PendingApprovalView>,
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
    /// Sessions list (answers to `list_sessions`).
    pub sessions: Vec<SessionMetaWire>,
    /// Scheduled automations (answers to `list_automations`; settings
    /// views render these).
    pub automations: Vec<modron_protocol::AutomationWire>,
    /// Provider auth rows (answers to `auth_providers`; the settings
    /// providers card renders these).
    pub providers: Vec<modron_protocol::AuthProviderWire>,
    /// Last seen server seq for this session (reconnect resumes past it).
    pub last_seq: u64,
    pub turns_completed: u32,
    pub tool_calls_total: u32,
    /// Last `Error` frame or disconnect cause (transient toast material).
    pub last_error: Option<String>,
    /// Last `Note` frame text (slash-command results).
    pub last_note: Option<String>,
    pub oauth: Option<OAuthProgressView>,
    /// Committed transcript, in order.
    pub history: Vec<TranscriptEntry>,
    /// Entries not yet drained by `take_new_entries`.
    pub new_entries: Vec<TranscriptEntry>,
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
    /// Tool allow/deny lists (editable in the settings views).
    pub allow_tools: Vec<String>,
    pub deny_tools: Vec<String>,
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
        let cache_denom =
            vm.cumulative_usage.input_tokens + vm.cumulative_usage.cached_input_tokens;
        let session_cache_pct = (cache_denom > 0)
            .then(|| 100.0 * vm.cumulative_usage.cached_input_tokens as f64 / cache_denom as f64);
        let mut status_line = if vm.model.is_empty() {
            "modron".to_string()
        } else {
            vm.model.clone()
        };
        status_line.push_str(&format!(" · {} tok", vm.cumulative_usage.total()));
        if cache_denom > 0 {
            status_line.push_str(&format!(
                " · cache {:.0}%",
                100.0 * vm.cumulative_usage.cached_input_tokens as f64 / cache_denom as f64
            ));
        }
        status_line.push_str(&format!(" · {}", approval_mode_label(vm.approval_mode)));
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
            tool_lines: vm.turn_tools.iter().map(ToolEvent::summary).collect(),
            turn_summary_line: vm.turn_summary_line(),
            subagent_rows: vm.subagents.iter().map(SwarmRow::summary).collect(),
            pending_approval: vm.pending_approval.clone(),
            approval_mode: vm.approval_mode,
            approval_mode_label: approval_mode_label(vm.approval_mode).to_string(),
            sandbox: sandbox_label(vm.sandbox).to_string(),
            allow_tools: vm.allow_tools.clone(),
            deny_tools: vm.deny_tools.clone(),
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
            last_seq: vm.last_seq,
            last_error: vm.last_error.clone(),
            last_note: vm.last_note.clone(),
            oauth: vm.oauth.clone(),
            history: vm
                .history
                .iter()
                .map(|entry| RenderedRow {
                    entry: entry.clone(),
                    lines: entry_lines(entry),
                })
                .collect(),
        }
    }
}

// ---------------------------------------------------------------------------
// Incremental folding state (Event frames through replay())
// ---------------------------------------------------------------------------

/// Bookkeeping for folding live `Event` frames through
/// [`modron_view::replay`] one event at a time while keeping replay's
/// grouping identical: `pending_log` is the naming prefix (the most recent
/// assistant `Message`, which carries `tool_calls`) plus the current
/// unflushed event group; `prefix_entries` counts the rows replay yields
/// for the prefix alone so those are skipped on every re-replay. A
/// trailing `ToolSummary` is held open (`held`) until the next flush
/// point, exactly like replay's end-of-log flush.
#[derive(Default)]
pub(crate) struct FoldState {
    pub tracker: ActivityTracker,
    pub prefix: Vec<SessionEvent>,
    pub pending_log: Vec<SessionEvent>,
    pub prefix_entries: usize,
    pub held: Option<TranscriptEntry>,
    /// Whether this turn already committed a tool summary (via the
    /// Event/ToolResult path); guards the live-strip commit at turn end
    /// against servers that send both.
    pub committed_tools: bool,
}

impl FoldState {
    /// Commit what is held (end of the current tool group), reset the log
    /// to the naming prefix.
    pub fn flush_tools(&mut self, vm: &mut SessionViewModel) {
        if let Some(entry) = self.held.take() {
            if matches!(entry, TranscriptEntry::ToolSummary { .. }) {
                self.committed_tools = true;
            }
            commit(vm, entry);
        }
        self.reset_to_prefix();
    }

    fn reset_to_prefix(&mut self) {
        self.pending_log = self.prefix.clone();
        self.prefix_entries = replay(&self.pending_log).len();
    }

    /// Fold one live `SessionEvent` into committed rows.
    pub fn fold_event(&mut self, vm: &mut SessionViewModel, event: SessionEvent) {
        // A persisted assistant message supersedes the streamed buffer
        // (the replayed row carries the authoritative text).
        if let SessionEvent::Message { message, .. } = &event {
            if message.role == Role::Assistant {
                vm.streaming_text.clear();
                vm.streaming_reasoning.clear();
            }
        }
        let is_tool_result = matches!(event, SessionEvent::ToolResult { .. });
        if !is_tool_result {
            // replay flushes an open tool group before every non-tool row.
            self.flush_tools(vm);
        }
        self.pending_log.push(event.clone());
        let entries = replay(&self.pending_log);
        let skip = self.prefix_entries.min(entries.len());
        // A trailing ToolSummary stays open (more results may follow).
        let held = match entries.last() {
            Some(TranscriptEntry::ToolSummary { .. }) => entries.last().cloned(),
            _ => None,
        };
        let committed_end = entries.len() - usize::from(held.is_some());
        for entry in &entries[skip..committed_end] {
            commit(vm, entry.clone());
        }
        self.held = held;
        if self.held.is_none() {
            // Group fully flushed; the naming prefix rolls forward.
            if let SessionEvent::Message { message, .. } = &event {
                if message.role == Role::Assistant {
                    self.prefix = vec![event.clone()];
                }
            }
            self.reset_to_prefix();
        }
        // Usage accounting: persisted Usage events are ground truth.
        if let SessionEvent::Usage { usage, .. } = event {
            vm.cumulative_usage.input_tokens += usage.input_tokens;
            vm.cumulative_usage.output_tokens += usage.output_tokens;
            vm.cumulative_usage.cached_input_tokens += usage.cached_input_tokens;
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
                })
                .collect(),
        };
        vm.turn_tools = self.tracker.turn_tools.clone();
        vm.subagents = self.tracker.subagents.clone();
        self.prefix.clear();
        self.pending_log.clear();
        self.prefix_entries = 0;
        self.held = None;
        self.committed_tools = false;
    }

    /// Fold one live `Tool` frame through the canonical tracker.
    pub fn fold_tool(&mut self, vm: &mut SessionViewModel, activity: &ToolActivity) {
        self.tracker.on_tool(activity);
        vm.turn_tools = self.tracker.turn_tools.clone();
        vm.subagents = self.tracker.subagents.clone();
    }
}

/// Append a committed row to history and the drain queue.
pub(crate) fn commit(vm: &mut SessionViewModel, entry: TranscriptEntry) {
    vm.history.push(entry.clone());
    vm.new_entries.push(entry);
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
    let entries = replay(events);
    vm.history = entries.clone();
    vm.new_entries = entries;
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
    vm.streaming_reasoning = live.streaming_reasoning.clone();
    vm.running = live.running;
    vm.model = live.model.clone();
    vm.provider = live.provider.clone();
    vm.goal = live.goal.clone();
    vm.cumulative_usage = live.cumulative_usage;
    vm.pending_approval = live
        .pending_approval
        .clone()
        .map(|PendingApproval { request_id, call }| PendingApprovalView::new(request_id, &call));
    fold.seed_from_live(vm, live);
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
    let last_assistant = vm.history.iter().rev().find_map(|entry| match entry {
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
    vm.running = false;
    vm.pending_approval = None;
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
    vm.running = false;
    vm.pending_approval = None;
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
    fn decision_labels_match_the_tui_words() {
        assert_eq!(DECISION_LABELS[0], (Decision::AllowOnce, "allow once"));
        assert_eq!(
            DECISION_LABELS[1],
            (Decision::AllowAll, "allow for this run")
        );
        assert_eq!(DECISION_LABELS[2], (Decision::Deny, "deny"));
        assert_eq!(approval_mode_label(None), "ask");
        assert_eq!(
            approval_mode_label(Some(ApprovalModeWire::ApproveAll)),
            "yolo"
        );
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
        let incremental = vm.history.clone();
        let whole = replay(&events);
        assert_eq!(incremental, whole, "incremental fold == one-shot replay");
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
        assert_eq!(snap.status_line, "gpt-5.6-sol · 400 tok · cache 25% · yolo");
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
        assert_eq!(rows, vec!["100 tok · cache 25%"]);
    }
}
