//! [`AppState`] -- protocol frames in, UI projections out.
//!
//! Plain Rust, no gpui: the UI layer is a thin renderer over this struct
//! and the tests assert the TUI-parity projections verbatim (see
//! AGENTS.md's UI/UX parity contract).
//!
//! Grouping and copy rules (repo law): transcript rows come from
//! [`modron_view::replay`] only; live tool/swarm views come from
//! [`modron_view::ActivityTracker`] only; every formatted string a row
//! shows is produced by modron-view ([`modron_view::ToolEvent::summary`],
//! [`modron_view::tool_summary_line`], [`modron_view::SwarmRow::summary`])
//! or mirrors the TUI's exact wording (approval decisions "allow once /
//! allow all / deny", mode names "ask / yolo / deny", the status strip's
//! `* N tok * cache N%`, the `warn: allow <tool> <args>?` prompt).

use std::collections::VecDeque;

use modron_core::{SessionEvent, ToolActivity, Usage};
use modron_protocol::{
    ApprovalModeWire, ConfigPatch, Decision, Frame, LiveState, Payload, PendingApproval,
    SessionMetaWire, WireSwarmRow,
};
use modron_view::{ActivityTracker, ReplayFolder, SwarmRow, ToolEvent, TranscriptEntry};

/// Connection state shown in the status bar at all times.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ConnState {
    /// Not connected (initial, or the socket dropped).
    #[default]
    Offline,
    /// Dialing/handshaking.
    Connecting,
    /// Handshake completed (`hello`/`welcome`).
    Connected { server_version: String },
}

/// One sidebar row (the protocol's `SessionMetaWire`, newest first).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRow {
    pub id: uuid::Uuid,
    pub title: String,
    pub updated_at: String,
    pub message_count: u32,
    /// Set when the session is a worktree task: the task branch.
    pub task_branch: Option<String>,
    /// "running" | "ready" (list-time snapshot).
    pub task_state: Option<String>,
    /// The folder the session runs in (sidebar grouping key; `None` on
    /// servers predating the field).
    pub working_dir: Option<String>,
}

impl From<&SessionMetaWire> for SessionRow {
    fn from(m: &SessionMetaWire) -> Self {
        Self {
            id: m.id,
            task_branch: m.task_branch.clone(),
            task_state: m.task_state.clone(),
            working_dir: m.working_dir.clone(),
            title: m.title.clone(),
            updated_at: m.updated_at.clone(),
            message_count: m.message_count,
        }
    }
}

/// One transcript row: a committed [`TranscriptEntry`] or the live
/// in-flight buffers (parity: assistant text streams into a visible
/// in-flight buffer; reasoning only when the user opted in; buffers
/// commit at TurnDone and clear on abort).
#[derive(Clone, Debug, PartialEq)]
pub enum Row {
    Entry(TranscriptEntry),
    /// A centered separator chip (model switches, compaction). The GUI
    /// folds per-turn Model/Usage notes into these chips + the status bar
    /// (a documented divergence: the TUI renders them as scrollback
    /// lines; a chat surface reads better without them between turns).
    Divider {
        text: String,
    },
    Streaming {
        text: String,
    },
    Reasoning {
        text: String,
    },
}

/// The whole UI state. Every protocol frame lands here via
/// [`AppState::apply_frame`]; renderers read the projections.
#[derive(Debug, Default)]
pub struct AppState {
    /// Provider auth status rows (onboarding/settings).
    pub auth_providers: Vec<modron_protocol::AuthProviderWire>,
    /// The router recommendation + fallbacks (settings defaults card).
    pub router_defaults: Option<modron_protocol::RouterDefaults>,
    /// Scheduled automations (wire rows).
    pub automations: Vec<modron_protocol::AutomationWire>,
    /// Provider auth status rows (onboarding wizard/settings).pub auth_providers: Vec<modron_protocol::AuthProviderWire>,
    pub conn: ConnState,
    pub sessions: Vec<SessionRow>,
    /// The attached session, if any.
    pub attached: Option<uuid::Uuid>,
    /// The attached session's last seen `seq` (re-attach resume point).
    pub last_seq: u64,
    /// Typewriter cursor into `streaming_text` (char count shown).
    stream_cursor: usize,
    /// Byte offset of the cursor (the incremental typewriter walk starts
    /// here instead of re-scanning the buffer from zero every tick).
    stream_byte: usize,
    /// Char count of `streaming_text` (maintained on push; `chars().count()`
    /// per tick was O(streamed) per frame).
    stream_chars: usize,
    /// The displayed prefix of `streaming_text` (extended in place as the
    /// cursor advances; `displayed_streaming` clones this instead of
    /// re-slicing the whole buffer).
    displayed_cache: String,

    events: Vec<SessionEvent>,
    /// Incremental replay state: one event per frame, never a full pass
    /// (the full fold only runs on snapshot reset).
    folder: ReplayFolder,
    /// Committed rows (the presentation fold of the replayed entries):
    /// maintained incrementally per folded event so a frame never
    /// re-clones the whole history (2026-11-12 audit perf fix;
    /// `rebuild_transcript` is the only full pass, on snapshot reset).
    committed_rows: Vec<Row>,
    /// Index into `committed_rows` where the LAST applied Event frame's
    /// rows begin; [`AppState::take_new_rows`] drains from here so the
    /// renderer splices its list incrementally instead of rebuilding.
    last_fold_start: usize,
    /// Last model name seen by the row fold (divider chips only on change).
    last_model: Option<String>,

    pub streaming_text: String,
    pub streaming_reasoning: String,
    /// The user's opt-in for rendering streamed reasoning (TUI /reasoning).
    pub show_reasoning: bool,

    pub tracker: ActivityTracker,
    /// The approval the strip shows (the FRONT of the queue).
    pub pending_approval: Option<PendingApproval>,
    /// Approvals waiting behind the front, FIFO. Invariant: empty while
    /// `pending_approval` is `None`.
    pub queued_approvals: VecDeque<PendingApproval>,

    pub model: String,
    pub provider: String,
    /// Approval-mode label (`ask`/`yolo`/`deny`; `ask` = engine default).
    pub approval_mode: String,
    /// The sandbox label (settings control; the last /sandbox patch this
    /// client sent; the server keeps the wire truth).
    pub sandbox: String,
    /// Standing goal (snapshot `live.goal`).
    pub goal: String,

    pub cumulative: Usage,
    pub running: bool,
    /// Completed turns and tool calls (status-bar counts).
    pub turn_count: u32,
    pub tool_count: u32,
    /// Latest transient note (the TUI's toast slot).
    pub last_note: Option<String>,
}

/// The three approval decisions, in the shared wording (parity: buttons
/// carry exactly these words).
pub fn decision_label(decision: Decision) -> &'static str {
    match decision {
        Decision::AllowOnce => "allow once",
        Decision::AllowAll => "allow all",
        Decision::Deny => "deny",
    }
}

impl AppState {
    pub fn new() -> Self {
        Self {
            approval_mode: "ask".into(),
            ..Self::default()
        }
    }

    // -- frame folding ----------------------------------------------------

    /// Fold one protocol frame (parity state mapping: Snapshot resets,
    /// Delta appends, Tool updates the tracker, ApprovalRequested blocks
    /// on a decision, ConfigChanged updates labels).
    pub fn apply_frame(&mut self, frame: Frame) {
        if frame.seq > 0 && frame.seq > self.last_seq {
            self.last_seq = frame.seq;
        }
        match frame.payload {
            Payload::Welcome { server_version, .. } => {
                self.conn = ConnState::Connected { server_version };
            }
            Payload::Snapshot {
                last_seq,
                events,
                live,
                ..
            } => {
                self.reset_from_snapshot(last_seq, events, live);
            }
            Payload::Event { event } => {
                // Incremental: fold the single new event into the replay
                // folder and the committed rows instead of replaying the
                // whole log (2026-11-12 audit perf fix). `last_fold_start`
                // marks what THIS frame added; the renderer drains it with
                // [`Self::take_new_rows`] and splices its list.
                let committed_before = self.committed_rows.len();
                let folded = self.folder.push(&event).to_vec();
                self.events.push(event);
                self.last_fold_start = committed_before;
                for entry in folded {
                    self.push_entry_row(entry);
                }
            }
            Payload::Delta { delta } => match delta {
                modron_protocol::WireDelta::Text { text } => {
                    self.streaming_text.push_str(&text);
                    self.stream_chars += text.chars().count();
                }
                modron_protocol::WireDelta::Reasoning { text } => {
                    self.streaming_reasoning.push_str(&text)
                }
                modron_protocol::WireDelta::Done => {}
            },
            Payload::Tool { activity } => {
                let core: ToolActivity = activity.into();
                self.tracker.on_tool(&core);
            }
            Payload::ApprovalRequested { request_id, call } => {
                self.enqueue_approval(PendingApproval { request_id, call });
            }
            Payload::ApprovalResolved { request_id, .. } => {
                // First responder wins; the echo clears that request everywhere.
                self.resolve_approval(&request_id);
            }
            Payload::ConfigChanged { patch } => self.apply_config_patch(&patch),
            Payload::Sessions { sessions } => {
                self.sessions = sessions.iter().map(SessionRow::from).collect();
                self.sessions.sort_by(|a, b| {
                    b.updated_at
                        .cmp(&a.updated_at)
                        .then(b.id.to_string().cmp(&a.id.to_string()))
                });
            }
            Payload::TurnDone { summary } => {
                self.running = false;
                self.cumulative.input_tokens += summary.usage.input_tokens;
                self.cumulative.output_tokens += summary.usage.output_tokens;
                self.cumulative.cached_input_tokens += summary.usage.cached_input_tokens;
                self.turn_count += summary.turns;
                self.tool_count += summary.tool_calls;
                // Commit: the run's events are already persisted (and
                // replayed); the live buffers and the per-turn strip land
                // as replay rows, the swarm block commits here.
                self.clear_buffers();
                self.clear_approvals();
                let swarm = self.tracker.take_swarm_summary();
                if !swarm.is_empty() {
                    let entry = TranscriptEntry::Swarm { lines: swarm };
                    self.push_entry_row(entry);
                }
                self.tracker.clear_turn();
            }
            Payload::TurnFailed { error } => {
                self.running = false;
                self.clear_buffers();
                self.clear_approvals();
                self.tracker.clear_turn();
                self.last_note = Some(error);
            }
            Payload::TurnAborted {} => {
                // Parity: buffers clear on abort; partial events stay
                // persisted (the replay already shows them).
                self.running = false;
                self.clear_buffers();
                self.clear_approvals();
                self.tracker.clear_turn();
            }
            // The client's own delete request echoes back; ignore it.
            Payload::DeleteSession { .. } => {}
            // Task lifecycle requests are client->server only. The
            // TaskCreated reply adopts the fresh session: submit + status
            // land there without an explicit attach.
            Payload::CreateTask { .. }
            | Payload::GetTaskStatus { .. }
            | Payload::MergeTask { .. } => {}
            Payload::TaskCreated { session_id, .. } => self.attached = Some(session_id),
            Payload::TaskStatus { .. } | Payload::TaskMerged { .. } => {}
            // Remote-control registration is TUI-driven this tranche
            // (divergence row in AGENTS.md); the desktop share button
            // lands next.
            Payload::RemoteControl { .. } => {}
            Payload::RemoteControlStarted { url } => {
                self.last_note = Some(format!("remote-control live: {url} - open on any device"));
            }
            Payload::PagePublished { url } => {
                self.last_note = Some(format!("published: {url}"));
            }
            Payload::AuthProviders { rows } => self.auth_providers = rows.clone(),
            // The request shape never flows server->client.
            Payload::AuthProvidersReq {} => {}
            Payload::Automations { rows } => self.automations = rows.clone(),
            // Client->server lifecycle frames echo nothing; the GUI drives
            // these itself via commands.
            Payload::AutomationsList { .. }
            | Payload::AutomationAdd { .. }
            | Payload::AutomationRemove { .. }
            | Payload::PersistRouter { .. }
            | Payload::PublishPage { .. } => {}
            Payload::Note { text } => {
                self.last_note = Some(text.clone());
                // `new_session`'s correlated reply names the fresh session
                // in its session field: adopt it so the next submit lands
                // there without an explicit attach.
                if let (Some(id), true) = (frame.session, text.starts_with("session created: ")) {
                    self.attached = Some(id);
                }
            }
            Payload::OAuthProgress {
                provider,
                stage,
                message,
                url,
                user_code,
            } => {
                let mut line = format!("oauth {provider}: {stage} — {message}");
                if let Some(code) = user_code {
                    line.push_str(&format!(" ({code})"));
                }
                if let Some(url) = url {
                    line.push_str(&format!(" {url}"));
                }
                self.last_note = Some(line);
            }
            Payload::Error { message, .. } => self.last_note = Some(message),
            Payload::Pong {} => {}
            // Client-only payloads never arrive inbound.
            Payload::RouterDefaults {
                authed,
                recommended_cheap,
                recommended_strong,
                cheap_fallback,
                strong_fallback,
            } => {
                self.router_defaults = Some(modron_protocol::RouterDefaults {
                    authed,
                    recommended_cheap,
                    recommended_strong,
                    cheap_fallback,
                    strong_fallback,
                });
            }
            Payload::RouterDefaultsReq {}
            | Payload::AuthStart { .. }
            | Payload::AuthSetKey { .. } => {}
            Payload::Hello { .. }
            | Payload::Attach { .. }
            | Payload::Detach {}
            | Payload::Submit { .. }
            | Payload::Interrupt { .. }
            | Payload::Approve { .. }
            | Payload::SetConfig { .. }
            | Payload::Fork { .. }
            | Payload::NewSession { .. }
            | Payload::ListSessions {}
            | Payload::Ping {} => {}
        }
    }

    fn reset_from_snapshot(&mut self, last_seq: u64, events: Vec<SessionEvent>, live: LiveState) {
        self.events = events;
        self.rebuild_transcript();
        self.last_seq = last_seq;
        self.streaming_text = live.streaming_text;
        self.streaming_reasoning = live.streaming_reasoning;
        self.tracker = ActivityTracker::default();
        self.tracker.turn_tools = live.turn_tools;
        self.tracker.subagents = live.subagents.iter().map(swarm_row_from_wire).collect();
        self.pending_approval = live.pending_approval;
        self.queued_approvals.clear();
        self.cumulative = live.cumulative_usage;
        self.model = live.model;
        self.provider = live.provider;
        self.goal = live.goal;
        self.running = live.running;
        // C5: the server's real mode and sandbox, through the same label
        // mappings ConfigChanged uses. Absent keeps the client's default.
        if let Some(mode) = live.approval_mode {
            self.approval_mode = approval_mode_label(mode).to_string();
        }
        if let Some(policy) = live.sandbox {
            self.sandbox = sandbox_label(policy).to_string();
        }
        self.resync_stream_cache();
    }

    fn rebuild_transcript(&mut self) {
        self.folder = ReplayFolder::new();
        for event in &self.events {
            self.folder.push(event);
        }
        // One full fold pass; afterwards every frame extends incrementally.
        self.committed_rows.clear();
        self.last_fold_start = 0;
        self.last_model = None;
        let entries = self.folder.entries().to_vec();
        for entry in entries {
            self.push_entry_row(entry);
        }
    }

    /// The presentation fold for ONE committed entry (the exact per-entry
    /// logic of the former whole-transcript `rows()` walk; parity test
    /// `incremental_rows_match_full_fold` pins them together). Takes
    /// ownership: the entry moves into its row instead of being cloned
    /// (2026-11-12 audit perf fix — the Event path cloned each new entry
    /// three times).
    fn push_entry_row(&mut self, entry: TranscriptEntry) {
        match entry {
            TranscriptEntry::Model { model, .. } => {
                if self.last_model.as_deref() != Some(model.as_str()) {
                    self.committed_rows.push(Row::Divider {
                        text: format!("model: {model}"),
                    });
                    self.last_model = Some(model);
                }
            }
            TranscriptEntry::Usage { .. } => {}
            TranscriptEntry::Plan { items } => {
                self.committed_rows
                    .push(Row::Entry(TranscriptEntry::Plan { items }));
            }
            TranscriptEntry::Compaction {
                summarized_events, ..
            } => self.committed_rows.push(Row::Divider {
                text: format!("compacted {summarized_events} earlier events"),
            }),
            other => self.committed_rows.push(Row::Entry(other)),
        }
    }

    /// Recompute the typewriter caches from the raw buffer (snapshot reset
    /// and commit paths; the buffer arrived whole, cursor resets to 0).
    fn resync_stream_cache(&mut self) {
        self.stream_chars = self.streaming_text.chars().count();
        self.stream_cursor = self.stream_chars;
        self.stream_byte = self.streaming_text.len();
        self.displayed_cache = self.streaming_text.clone();
    }

    fn clear_buffers(&mut self) {
        self.streaming_text.clear();
        self.stream_cursor = 0;
        self.stream_byte = 0;
        self.stream_chars = 0;
        self.displayed_cache.clear();
        self.streaming_reasoning.clear();
    }

    /// Whether a request id is already the front or queued (dedupe key).
    fn approval_held(&self, request_id: &str) -> bool {
        self.pending_approval
            .as_ref()
            .is_some_and(|p| p.request_id == request_id)
            || self
                .queued_approvals
                .iter()
                .any(|p| p.request_id == request_id)
    }

    /// A new approval request: the front slot when it is free, else FIFO
    /// behind it. A request id already held is ignored.
    fn enqueue_approval(&mut self, pending: PendingApproval) {
        if self.approval_held(&pending.request_id) {
            return;
        }
        if self.pending_approval.is_none() {
            self.pending_approval = Some(pending);
        } else {
            self.queued_approvals.push_back(pending);
        }
    }

    /// Resolve one request by id (the server echo, or the local clear after
    /// a click). The front matching promotes the next queued request; a
    /// queued match is removed; anything else is a no-op.
    pub fn resolve_approval(&mut self, request_id: &str) {
        if self
            .pending_approval
            .as_ref()
            .is_some_and(|p| p.request_id == request_id)
        {
            self.pending_approval = self.queued_approvals.pop_front();
        } else if let Some(ix) = self
            .queued_approvals
            .iter()
            .position(|p| p.request_id == request_id)
        {
            self.queued_approvals.remove(ix);
        }
    }

    /// Drop every approval (the run ended, so none can be answered).
    fn clear_approvals(&mut self) {
        self.pending_approval = None;
        self.queued_approvals.clear();
    }

    /// The socket dropped or never came up: nothing runs, no approval can
    /// be answered, and the in-flight buffers can no longer complete
    /// (AGENTS.md invariant 5: buffers clear on abort).
    pub fn on_disconnected(&mut self) {
        self.conn = ConnState::Offline;
        self.running = false;
        self.clear_approvals();
        self.clear_buffers();
        self.tracker.clear_turn();
    }

    fn apply_config_patch(&mut self, patch: &ConfigPatch) {
        // Future-turn semantics live server-side; the labels just follow.
        if let Some(model) = &patch.model {
            self.model = model.clone();
        }
        if let Some(provider) = &patch.provider {
            self.provider = provider.clone();
        }
        if let Some(mode) = patch.approval_mode {
            self.approval_mode = approval_mode_label(mode).to_string();
        }
        if let Some(policy) = patch.sandbox {
            self.sandbox = sandbox_label(policy).to_string();
        }
        if let Some(goal) = &patch.goal {
            self.goal = goal.clone();
        }
    }

    // -- projections -------------------------------------------------------

    /// The committed rows (presentation fold of the transcript): owned by
    /// the state and maintained incrementally, so the renderer can borrow
    /// them instead of re-cloning the history per frame.
    pub fn committed_rows(&self) -> &[Row] {
        &self.committed_rows
    }

    /// The rows the LAST applied Event frame added to the committed rows,
    /// draining the marker (a second call returns nothing until the next
    /// Event frame). The renderer consumes this right after
    /// [`Self::apply_frame`] to splice its list incrementally — O(new
    /// rows), not the former O(n) rebuild per persisted event. Snapshot
    /// and turn-boundary frames rebuild the display wholesale instead.
    pub fn take_new_rows(&mut self) -> Vec<Row> {
        let new_rows = self.committed_rows[self.last_fold_start..].to_vec();
        self.last_fold_start = self.committed_rows.len();
        new_rows
    }

    /// The live tail rows (opted-in reasoning + the typewriter streaming
    /// buffer): small, built per call.
    pub fn tail_rows(&self) -> Vec<Row> {
        let mut rows = Vec::with_capacity(2);
        if self.show_reasoning && !self.streaming_reasoning.is_empty() {
            rows.push(Row::Reasoning {
                text: self.streaming_reasoning.clone(),
            });
        }
        let visible_stream = self.displayed_streaming();
        if !visible_stream.is_empty() {
            rows.push(Row::Streaming {
                text: visible_stream,
            });
        }
        rows
    }

    /// Committed transcript plus the in-flight buffers as rows (what the
    /// virtualized list renders). The renderer hot path uses
    /// [`Self::committed_rows`] + [`Self::tail_rows`]; this remains for
    /// tests and one-shot callers.
    pub fn rows(&self) -> Vec<Row> {
        let mut rows = self.committed_rows.clone();
        rows.extend(self.tail_rows());
        rows
    }

    /// The streaming text the user should SEE right now: the buffer up to
    /// the typewriter cursor (character-level streaming - deltas land in
    /// the buffer at chunk granularity, the display advances per char).
    /// Maintained incrementally by [`Self::advance_stream`].
    pub fn displayed_streaming(&self) -> String {
        self.displayed_cache.clone()
    }

    /// Advance the typewriter cursor (adaptive: bigger steps the further
    /// behind the display falls). Returns whether anything moved.
    pub fn advance_stream(&mut self) -> bool {
        if self.stream_cursor >= self.stream_chars {
            return false;
        }
        let behind = self.stream_chars - self.stream_cursor;
        let step = (behind / 12).max(2);
        let new_cursor = (self.stream_cursor + step).min(self.stream_chars);
        // Walk `new_cursor - stream_cursor` chars forward from the cached
        // byte offset and extend the displayed prefix in place - O(step),
        // not O(streamed).
        let old_byte = self.stream_byte;
        let steps = new_cursor - self.stream_cursor;
        let mut new_byte = old_byte;
        for (i, b) in self.streaming_text[old_byte..].char_indices().take(steps) {
            new_byte = old_byte + i + b.len_utf8();
        }
        self.displayed_cache
            .push_str(&self.streaming_text[old_byte..new_byte]);
        self.stream_cursor = new_cursor;
        self.stream_byte = new_byte;
        true
    }

    /// The expandable tool panel: per-event rows ([`ToolEvent::summary`])
    /// plus the live swarm rows ([`SwarmRow::summary`]).
    pub fn tool_panel_rows(&self) -> Vec<String> {
        let mut rows: Vec<String> = self
            .tracker
            .turn_tools
            .iter()
            .map(ToolEvent::summary)
            .collect();
        rows.extend(self.tracker.subagents.iter().map(SwarmRow::summary));
        rows
    }

    /// The live strip: latest tool event summary + in-flight count
    /// (verbatim [`ActivityTracker::live_strip`]).
    pub fn live_strip(&self) -> Option<(String, usize)> {
        self.tracker.live_strip()
    }

    /// The `⏺ N tools * …` collapsed line for the current turn.
    pub fn turn_summary_line(&self) -> Option<String> {
        self.tracker.turn_summary_line()
    }

    /// The approval prompt while one is pending -- the TUI's exact wording
    /// minus the key hints (keys diverge; words do not).
    pub fn approval_prompt(&self) -> Option<String> {
        let pending = self.pending_approval.as_ref()?;
        Some(format!(
            "⚠ allow {} {}?",
            pending.call.name,
            modron_view::approval_summary(
                &pending.call.name,
                &pending.call.arguments,
                modron_view::APPROVAL_SUMMARY_MAX
            )
        ))
    }

    /// The one-line status strip -- the TUI's facts, desktop wording:
    /// connection, model/provider, cumulative tokens, cache hit rate,
    /// approval mode, turn/tool counts, running/idle.
    pub fn status_line(&self) -> String {
        let conn = match &self.conn {
            ConnState::Offline => "offline".to_string(),
            ConnState::Connecting => "connecting".to_string(),
            ConnState::Connected { server_version } => format!("connected ({server_version})"),
        };
        let mut parts = vec![conn];
        if !self.model.is_empty() {
            parts.push(self.model.clone());
        }
        if !self.provider.is_empty() {
            parts.push(self.provider.clone());
        }
        parts.push(format!("{} tok", self.cumulative.total()));
        if let Some(pct) = modron_view::cache_hit_pct(&self.cumulative) {
            parts.push(format!("cache {pct:.0}%"));
        }
        parts.push(self.approval_mode.clone());
        parts.push(format!("turns {}", self.turn_count));
        parts.push(format!("tools {}", self.tool_count));
        if self.running {
            parts.push(format!("turn {}", self.turn_count + 1));
            if let Some((latest, _)) = self.live_strip() {
                parts.push(latest);
            }
            parts.push("running".to_string());
        } else {
            parts.push("idle".to_string());
        }
        if !self.goal.is_empty() {
            parts.push(format!("goal: {}", modron_view::short(&self.goal, 40)));
        }
        parts.join(" · ")
    }

    /// The session id label, shortened like the TUI (`session 1234abcd`).
    pub fn session_label(&self) -> String {
        self.attached
            .map(|id| format!("session {}", &id.to_string()[..8]))
            .unwrap_or_else(|| "no session".into())
    }

    /// Optimistic running flag while a submit/interrupt is in flight.
    pub fn mark_running(&mut self) {
        self.running = true;
    }
}

/// The wire's swarm row into the view's (fields match one-for-one).
fn swarm_row_from_wire(w: &WireSwarmRow) -> SwarmRow {
    SwarmRow {
        id: w.id.clone(),
        label: w.label.clone(),
        turns: w.turns,
        tokens: w.tokens,
        last_tool: w.last_tool.clone(),
        done: w.done,
        started_ms: None,
    }
}

/// Wire mode names in the TUI's user-facing vocabulary (ask / yolo / deny).
pub fn approval_mode_label(mode: ApprovalModeWire) -> &'static str {
    match mode {
        ApprovalModeWire::Deny => "deny",
        ApprovalModeWire::Ask => "ask",
        ApprovalModeWire::ApproveAll => "yolo",
    }
}

/// The sandbox words the settings control shows (`off` / `seatbelt` /
/// `bwrap`). Shared by ConfigChanged and the attach snapshot.
pub fn sandbox_label(policy: modron_core::SandboxPolicy) -> &'static str {
    match policy {
        modron_core::SandboxPolicy::Seatbelt => "seatbelt",
        modron_core::SandboxPolicy::Bubblewrap => "bwrap",
        modron_core::SandboxPolicy::Off => "off",
    }
}

/// The `SetConfig` patch for a model switch: model-only. The server's
/// hub follows the provider through the shared catalog (mobile and the
/// TUI's remote control already send model-only patches); pinning it
/// client-side was a stale duplicate of the server's
/// [`modron_router::lookup`] (2026-11-12 audit DRY fix).
pub fn model_patch(model: &str) -> ConfigPatch {
    ConfigPatch {
        model: Some(model.to_string()),
        ..ConfigPatch::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use modron_core::{Message, ToolCall, ToolOutcome};
    use modron_protocol::{WireDelta, WireRunSummary, WireToolActivity, PROTOCOL_VERSION};
    use uuid::Uuid;

    fn sid() -> Uuid {
        Uuid::new_v4()
    }

    fn event_frame(seq: u64, payload: Payload) -> Frame {
        Frame::event(Some(sid()), seq, payload)
    }

    fn assistant_with_tool_call(id: &str, name: &str, text: &str) -> Message {
        let mut m = Message::assistant(text);
        m.tool_calls = vec![ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: serde_json::json!({"path": "a.rs"}),
        }];
        m
    }

    fn snapshot_frame(events: Vec<SessionEvent>, live: LiveState) -> Frame {
        Frame::event(
            Some(sid()),
            0,
            Payload::Snapshot {
                session_id: sid(),
                last_seq: events.len() as u64,
                events,
                live,
            },
        )
    }

    fn history_events() -> Vec<SessionEvent> {
        vec![
            SessionEvent::Task {
                text: "read a.rs".into(),
            },
            SessionEvent::Message {
                parent_id: None,
                message: assistant_with_tool_call("call-1", "read", "looking"),
            },
            SessionEvent::ToolResult {
                call_id: "call-1".into(),
                outcome: ToolOutcome::ok("file contents here"),
            },
            SessionEvent::Message {
                parent_id: None,
                message: Message::assistant("it says hi"),
            },
            SessionEvent::Usage {
                model: "m1".into(),
                usage: Usage {
                    input_tokens: 100,
                    output_tokens: 10,
                    cached_input_tokens: 40,
                },
            },
        ]
    }

    #[test]
    fn incremental_rows_match_full_replay_fold() {
        // The Event frame path folds one event at a time through the
        // ReplayFolder; a snapshot replay folds everything at once. The
        // committed rows must be identical, including the multi-tool-run
        // merge into ONE summary row.
        let events = vec![
            SessionEvent::Task {
                text: "fix it".into(),
            },
            SessionEvent::Message {
                parent_id: None,
                message: assistant_with_tool_call("c1", "read", ""),
            },
            SessionEvent::ToolResult {
                call_id: "c1".into(),
                outcome: ToolOutcome::ok("a"),
            },
            SessionEvent::Message {
                parent_id: None,
                message: assistant_with_tool_call("c2", "grep", ""),
            },
            SessionEvent::ToolResult {
                call_id: "c2".into(),
                outcome: ToolOutcome::err("b"),
            },
            SessionEvent::Message {
                parent_id: None,
                message: Message::assistant("done"),
            },
            SessionEvent::ModelSelected {
                model: "m2".into(),
                reason: "r".into(),
                action: None,
                context_tokens: None,
            },
            SessionEvent::ModelSelected {
                model: "m2".into(),
                reason: "r".into(),
                action: None,
                context_tokens: None,
            },
            SessionEvent::ModelSelected {
                model: "m3".into(),
                reason: "r".into(),
                action: None,
                context_tokens: None,
            },
        ];
        let mut incremental = AppState::new();
        for (i, event) in events.iter().enumerate() {
            incremental.apply_frame(event_frame(
                i as u64 + 1,
                Payload::Event {
                    event: event.clone(),
                },
            ));
        }
        let mut bulk = AppState::new();
        bulk.apply_frame(snapshot_frame(events.clone(), LiveState::default()));
        assert_eq!(
            incremental.rows(),
            bulk.rows(),
            "folded incrementally == folded from a snapshot"
        );
        // Two tool runs (each assistant message closes the run - replay
        // law, pinned identically by both folds above), then divider chips
        // only on model CHANGE.
        let rows = incremental.rows();
        assert_eq!(
            rows.iter()
                .filter(|r| matches!(r, Row::Entry(TranscriptEntry::ToolSummary { .. })))
                .count(),
            2,
            "one summary per tool run"
        );
        assert_eq!(
            rows.iter()
                .filter(|r| matches!(r, Row::Divider { .. }))
                .count(),
            2,
            "m2 m2 m3 folds to two chips (m2 once, m3 once)"
        );
    }

    #[test]
    fn typewriter_advances_multibyte_text_exactly() {
        let mut s = AppState::new();
        s.apply_frame(event_frame(
            1,
            Payload::Delta {
                delta: WireDelta::Text {
                    text: "h\u{e9}llo w\u{f6}rld \u{1f40d}".into(),
                },
            },
        ));
        // Reveal everything one step at a time; the prefix must always be
        // valid char boundaries and end at the whole text.
        let mut last = String::new();
        loop {
            if !s.advance_stream() {
                break;
            }
            let shown = s.displayed_streaming();
            assert!(s.streaming_text.starts_with(&shown), "prefix invariant");
            last = shown;
        }
        assert_eq!(last, s.streaming_text);
        assert!(!s.advance_stream(), "exhausted cursor stays put");
    }

    #[test]
    fn welcome_connects() {
        let mut s = AppState::new();
        assert_eq!(s.conn, ConnState::Offline);
        s.apply_frame(Frame::event(
            None,
            0,
            Payload::Welcome {
                server_version: "demo".into(),
                protocol: PROTOCOL_VERSION,
            },
        ));
        assert_eq!(
            s.conn,
            ConnState::Connected {
                server_version: "demo".into()
            }
        );
    }

    #[test]
    fn snapshot_replays_and_seeds_live_state() {
        let mut s = AppState::new();
        s.apply_frame(snapshot_frame(
            history_events(),
            LiveState {
                running: true,
                streaming_text: "partial ans".into(),
                model: "claude-haiku-5-5".into(),
                provider: "anthropic".into(),
                cumulative_usage: Usage {
                    input_tokens: 100,
                    output_tokens: 10,
                    cached_input_tokens: 40,
                },
                ..LiveState::default()
            },
        ));
        // Committed transcript from replay(), in order.
        let rows = s.rows();
        assert!(
            matches!(&rows[0], Row::Entry(TranscriptEntry::Task { text }) if text == "read a.rs")
        );
        assert!(
            matches!(&rows[1], Row::Entry(TranscriptEntry::Assistant { text }) if text == "looking")
        );
        // The tool summary line comes verbatim from modron-view.
        assert!(matches!(
            &rows[2],
            Row::Entry(TranscriptEntry::ToolSummary { line, .. }) if line == "⏺ 1 tool · read"
        ));
        assert!(
            matches!(&rows[3], Row::Entry(TranscriptEntry::Assistant { text }) if text == "it says hi")
        );
        // Streaming buffer trails as the in-flight row. The typewriter
        // cursor gates what is visible: reveal everything, then assert.
        for _ in 0..100 {
            if !s.advance_stream() {
                break;
            }
        }
        let rows = s.rows();
        assert_eq!(
            rows.last(),
            Some(&Row::Streaming {
                text: "partial ans".into()
            })
        );
        // Live labels + usage from the snapshot.
        assert!(s.running);
        assert_eq!(s.model, "claude-haiku-5-5");
        assert_eq!(s.provider, "anthropic");
        assert!(s.status_line().contains("110 tok"));
        assert!(s.status_line().contains("cache 40%"), "{}", s.status_line());
        assert_eq!(s.last_seq, 5);
    }

    #[test]
    fn deltas_stream_and_commit_at_turn_done() {
        let mut s = AppState::new();
        s.apply_frame(snapshot_frame(Vec::new(), LiveState::default()));
        s.apply_frame(event_frame(
            1,
            Payload::Delta {
                delta: WireDelta::Text { text: "hel".into() },
            },
        ));
        s.apply_frame(event_frame(
            2,
            Payload::Delta {
                delta: WireDelta::Text {
                    text: "lo world".into(),
                },
            },
        ));
        for _ in 0..100 {
            if !s.advance_stream() {
                break;
            }
        }
        assert_eq!(
            s.rows().last(),
            Some(&Row::Streaming {
                text: "hello world".into()
            })
        );
        // seq tracking follows the event stream.
        assert_eq!(s.last_seq, 2);

        s.apply_frame(event_frame(
            3,
            Payload::TurnDone {
                summary: WireRunSummary {
                    session_id: sid(),
                    answer: "hello world".into(),
                    turns: 1,
                    tool_calls: 2,
                    usage: Usage {
                        input_tokens: 50,
                        output_tokens: 5,
                        cached_input_tokens: 0,
                    },
                    models_used: vec!["m".into()],
                },
            },
        ));
        // Committed: the buffer row is gone; counts and usage landed.
        assert!(s
            .rows()
            .last()
            .is_none_or(|r| !matches!(r, Row::Streaming { .. })));
        assert!(!s.running);
        assert_eq!(s.turn_count, 1);
        assert_eq!(s.tool_count, 2);
        assert_eq!(s.cumulative.total(), 55);
        assert!(s.status_line().contains("turns 1"));
    }

    #[test]
    fn tool_frames_feed_the_tracker_with_shared_copy() {
        let mut s = AppState::new();
        s.apply_frame(event_frame(
            1,
            Payload::Tool {
                activity: WireToolActivity::Start {
                    call_id: None,
                    name: "read".into(),
                },
            },
        ));
        s.apply_frame(event_frame(
            2,
            Payload::Tool {
                activity: WireToolActivity::Start {
                    call_id: None,
                    name: "bash".into(),
                },
            },
        ));
        s.apply_frame(event_frame(
            3,
            Payload::Tool {
                activity: WireToolActivity::Done {
                    call_id: None,
                    name: "read".into(),
                    is_error: false,
                    bytes: 2150,
                },
            },
        ));
        // Verbatim modron-view copy, in ActivityTracker order.
        assert_eq!(
            s.tool_panel_rows(),
            vec!["✓ read — 2.1 KB".to_string(), "● bash …".to_string()]
        );
        assert_eq!(
            s.live_strip(),
            Some(("● bash …".to_string(), 2)),
            "strip shows the latest event"
        );
        assert_eq!(
            s.turn_summary_line().as_deref(),
            Some("⏺ 2 tools · bash · read")
        );
        s.apply_frame(event_frame(
            4,
            Payload::Tool {
                activity: WireToolActivity::SubagentStarted {
                    id: "a1".into(),
                    label: "explore".into(),
                },
            },
        ));
        s.apply_frame(event_frame(
            5,
            Payload::Tool {
                activity: WireToolActivity::SubagentProgress {
                    id: "a1".into(),
                    label: "explore".into(),
                    turns: 2,
                    tokens: 500,
                    last_tool: Some("grep".into()),
                },
            },
        ));
        assert!(s
            .tool_panel_rows()
            .iter()
            .any(|r| r == "⏵ [explore] running · 2 turns · 500 tok · last: grep"));
    }

    #[test]
    fn swarm_block_commits_at_turn_done() {
        let mut s = AppState::new();
        s.apply_frame(event_frame(
            1,
            Payload::Tool {
                activity: WireToolActivity::SubagentStarted {
                    id: "a1".into(),
                    label: "explore".into(),
                },
            },
        ));
        s.apply_frame(event_frame(
            2,
            Payload::Tool {
                activity: WireToolActivity::SubagentFinished {
                    id: "a1".into(),
                    label: "explore".into(),
                    tokens: 700,
                    turns: 3,
                    is_error: false,
                },
            },
        ));
        s.apply_frame(event_frame(
            3,
            Payload::TurnDone {
                summary: WireRunSummary {
                    session_id: sid(),
                    answer: String::new(),
                    turns: 1,
                    tool_calls: 0,
                    usage: Usage::default(),
                    models_used: vec![],
                },
            },
        ));
        let rows = s.rows();
        let last = rows.last().expect("swarm row lands");
        match last {
            Row::Entry(TranscriptEntry::Swarm { lines }) => {
                assert_eq!(lines.len(), 2, "header + one row");
                assert!(lines[0].starts_with("⎇ swarm: 1 agent(s), 700 tok"));
                assert_eq!(lines[1], "⏵ [explore] done · 3 turns · 700 tok · last: -");
            }
            other => panic!("expected swarm entry, got {other:?}"),
        }
        assert!(s.tracker.subagents.is_empty(), "committed and cleared");
    }

    #[test]
    fn approval_lifecycle_blocks_and_resolves() {
        let mut s = AppState::new();
        let call = ToolCall {
            id: "c1".into(),
            name: "edit".into(),
            arguments: serde_json::json!({"path": "src/lib.rs"}),
        };
        s.apply_frame(event_frame(
            1,
            Payload::ApprovalRequested {
                request_id: "req-9".into(),
                call: call.clone(),
            },
        ));
        assert_eq!(
            s.pending_approval.as_ref().map(|p| p.request_id.as_str()),
            Some("req-9")
        );
        // TUI's exact prompt wording.
        assert_eq!(
            s.approval_prompt().as_deref(),
            Some(r#"⚠ allow edit {"path":"src/lib.rs"}?"#)
        );
        // The decision labels are the shared copy.
        assert_eq!(decision_label(Decision::AllowOnce), "allow once");
        assert_eq!(decision_label(Decision::AllowAll), "allow all");
        assert_eq!(decision_label(Decision::Deny), "deny");
        // The echo (any decision) clears the strip.
        s.apply_frame(event_frame(
            2,
            Payload::ApprovalResolved {
                request_id: "req-9".into(),
                decision: Decision::AllowOnce,
            },
        ));
        assert!(s.pending_approval.is_none());
        assert!(s.approval_prompt().is_none());
    }

    #[test]
    fn snapshot_carries_the_server_mode_and_sandbox_labels() {
        // C5: a client attaching to a yolo session shows yolo, not the
        // default ask. (Not compiled on Linux; written for the macOS gate.)
        let mut s = AppState::new();
        let live = LiveState {
            approval_mode: Some(ApprovalModeWire::ApproveAll),
            sandbox: Some(modron_core::SandboxPolicy::Bubblewrap),
            ..LiveState::default()
        };
        s.apply_frame(snapshot_frame(Vec::new(), live));
        assert_eq!(s.approval_mode, "yolo");
        assert_eq!(s.sandbox, "bwrap");
        assert!(s.status_line().contains(" · yolo · "));

        // An old snapshot without the fields keeps the labels it has.
        s.apply_frame(snapshot_frame(Vec::new(), LiveState::default()));
        assert_eq!(s.approval_mode, "yolo");
        assert_eq!(s.sandbox, "bwrap");
    }

    #[test]
    fn config_changed_updates_labels_only() {
        let mut s = AppState::new();
        assert_eq!(s.approval_mode, "ask");
        s.apply_frame(event_frame(
            1,
            Payload::ConfigChanged {
                patch: ConfigPatch {
                    model: Some("gpt-5.6-sol".into()),
                    provider: Some("openai".into()),
                    approval_mode: Some(ApprovalModeWire::ApproveAll),
                    ..ConfigPatch::default()
                },
            },
        ));
        assert_eq!(s.model, "gpt-5.6-sol");
        assert_eq!(s.provider, "openai");
        assert_eq!(s.approval_mode, "yolo");
        assert!(s.status_line().contains(" · yolo · "));
        s.apply_frame(event_frame(
            2,
            Payload::ConfigChanged {
                patch: ConfigPatch {
                    approval_mode: Some(ApprovalModeWire::Deny),
                    ..ConfigPatch::default()
                },
            },
        ));
        assert_eq!(s.approval_mode, "deny");
    }

    #[test]
    fn model_patch_is_model_only() {
        // DELIBERATE change (2026-11-12 audit DRY fix): the patch no
        // longer pins the provider client-side — the server's hub follows
        // the provider through the catalog (mobile and TUI-remote already
        // send model-only). The demo echo test pins the server filling it
        // back in on ConfigChanged.
        let patch = model_patch("claude-haiku-5-5");
        assert_eq!(patch.model.as_deref(), Some("claude-haiku-5-5"));
        assert_eq!(patch.provider, None);
    }

    #[test]
    fn take_new_rows_drains_the_last_frames_rows() {
        let mut s = AppState::new();
        s.apply_frame(event_frame(
            1,
            Payload::Event {
                event: SessionEvent::Task { text: "hi".into() },
            },
        ));
        let fresh = s.take_new_rows();
        assert_eq!(fresh.len(), 1, "the task row is this frame's delta");
        assert!(s.take_new_rows().is_empty(), "drained after the first call");
        // A ToolResult accumulates: zero rows until a boundary event
        // flushes the tool summary.
        s.apply_frame(event_frame(
            2,
            Payload::Event {
                event: SessionEvent::ToolResult {
                    call_id: "c1".into(),
                    outcome: ToolOutcome::ok("x"),
                },
            },
        ));
        assert!(
            s.take_new_rows().is_empty(),
            "tool results ride the summary"
        );
        s.apply_frame(event_frame(
            3,
            Payload::Event {
                event: SessionEvent::Message {
                    parent_id: None,
                    message: Message::assistant("done"),
                },
            },
        ));
        let rows = s.take_new_rows();
        assert_eq!(
            rows.len(),
            2,
            "the boundary flushes the tool summary + the assistant row"
        );
    }

    #[test]
    fn abort_clears_buffers_but_keeps_persisted_rows() {
        let mut s = AppState::new();
        s.apply_frame(snapshot_frame(history_events(), LiveState::default()));
        s.apply_frame(event_frame(
            6,
            Payload::Delta {
                delta: WireDelta::Text {
                    text: "partial".into(),
                },
            },
        ));
        s.apply_frame(event_frame(7, Payload::TurnAborted {}));
        assert!(!s.running);
        assert!(s.streaming_text.is_empty());
        let rows = s.rows();
        assert!(rows.len() >= 4, "persisted rows survive: {rows:?}");
        assert!(!rows.iter().any(|r| matches!(r, Row::Streaming { .. })));
    }

    #[test]
    fn turn_failed_surfaces_as_note() {
        let mut s = AppState::new();
        s.mark_running();
        s.apply_frame(event_frame(
            1,
            Payload::Delta {
                delta: WireDelta::Text {
                    text: "half an".into(),
                },
            },
        ));
        s.apply_frame(event_frame(
            2,
            Payload::TurnFailed {
                error: "boom".into(),
            },
        ));
        assert!(!s.running);
        assert!(s.streaming_text.is_empty());
        assert_eq!(s.last_note.as_deref(), Some("boom"));
    }

    #[test]
    fn sessions_list_sorts_newest_first() {
        let mut s = AppState::new();
        s.apply_frame(Frame::event(
            None,
            0,
            Payload::Sessions {
                sessions: vec![
                    SessionMetaWire {
                        id: sid(),
                        title: "older".into(),
                        created_at: "2026-11-01T10:00:00Z".into(),
                        updated_at: "2026-11-01T10:00:00Z".into(),
                        message_count: 2,
                        task_branch: None,
                        task_state: None,
                        working_dir: None,
                    },
                    SessionMetaWire {
                        id: sid(),
                        title: "newest".into(),
                        created_at: "2026-11-09T10:00:00Z".into(),
                        updated_at: "2026-11-09T10:00:00Z".into(),
                        message_count: 5,
                        task_branch: None,
                        task_state: None,
                        working_dir: None,
                    },
                ],
            },
        ));
        assert_eq!(s.sessions[0].title, "newest");
        assert_eq!(s.sessions[1].title, "older");
    }

    #[test]
    fn status_line_carries_the_whole_surface() {
        let mut s = AppState::new();
        s.apply_frame(Frame::event(
            None,
            0,
            Payload::Welcome {
                server_version: "0.1.2".into(),
                protocol: PROTOCOL_VERSION,
            },
        ));
        s.apply_frame(event_frame(
            1,
            Payload::ConfigChanged {
                patch: ConfigPatch {
                    model: Some("glm-5.3".into()),
                    provider: Some("zai".into()),
                    ..ConfigPatch::default()
                },
            },
        ));
        s.apply_frame(event_frame(
            2,
            Payload::TurnDone {
                summary: WireRunSummary {
                    session_id: sid(),
                    answer: String::new(),
                    turns: 3,
                    tool_calls: 21,
                    usage: Usage {
                        input_tokens: 800,
                        output_tokens: 100,
                        cached_input_tokens: 200,
                    },
                    models_used: vec![],
                },
            },
        ));
        let line = s.status_line();
        for fact in [
            "connected (0.1.2)",
            "glm-5.3",
            "zai",
            "900 tok",
            "cache 25%",
            "ask",
            "turns 3",
            "tools 21",
            "idle",
        ] {
            assert!(line.contains(fact), "status `{line}` must show `{fact}`");
        }
    }

    #[test]
    fn snapshot_pending_approval_blocks_a_late_joiner() {
        let mut s = AppState::new();
        let live = LiveState {
            pending_approval: Some(PendingApproval {
                request_id: "req-1".into(),
                call: ToolCall {
                    id: "c".into(),
                    name: "write".into(),
                    arguments: serde_json::json!({"path": "x.txt"}),
                },
            }),
            ..LiveState::default()
        };
        s.apply_frame(snapshot_frame(Vec::new(), live));
        assert_eq!(s.approval_prompt().as_deref(), Some("⚠ allow write x.txt?"));
    }

    #[test]
    fn approval_prompt_uses_the_shared_summary_and_marks_truncation() {
        let mut s = AppState::new();
        let command = format!("echo {}", "a".repeat(120));
        s.pending_approval = Some(PendingApproval {
            request_id: "req-2".into(),
            call: ToolCall {
                id: "c".into(),
                name: "bash".into(),
                arguments: serde_json::json!({ "command": command.clone() }),
            },
        });
        let expected = format!(
            "⚠ allow bash {}?",
            modron_view::approval_summary("bash", &serde_json::json!({ "command": command }), 80)
        );
        assert_eq!(s.approval_prompt().as_deref(), Some(expected.as_str()));
        assert!(expected.ends_with(" [+45 chars]?"), "{expected}");
    }

    fn approval_request(id: &str) -> PendingApproval {
        PendingApproval {
            request_id: id.into(),
            call: ToolCall {
                id: format!("call-{id}"),
                name: "bash".into(),
                arguments: serde_json::json!({ "command": "ls" }),
            },
        }
    }

    fn requested_frame(seq: u64, id: &str) -> Frame {
        let call = approval_request(id).call;
        event_frame(
            seq,
            Payload::ApprovalRequested {
                request_id: id.into(),
                call,
            },
        )
    }

    fn resolved_frame(seq: u64, id: &str) -> Frame {
        event_frame(
            seq,
            Payload::ApprovalResolved {
                request_id: id.into(),
                decision: Decision::AllowOnce,
            },
        )
    }

    fn front_id(s: &AppState) -> Option<&str> {
        s.pending_approval.as_ref().map(|p| p.request_id.as_str())
    }

    fn queued_ids(s: &AppState) -> Vec<&str> {
        s.queued_approvals
            .iter()
            .map(|p| p.request_id.as_str())
            .collect()
    }

    #[test]
    fn approvals_queue_fifo_behind_the_front() {
        let mut s = AppState::new();
        s.apply_frame(requested_frame(1, "a"));
        s.apply_frame(requested_frame(2, "b"));
        s.apply_frame(requested_frame(3, "c"));
        assert_eq!(front_id(&s), Some("a"));
        assert_eq!(queued_ids(&s), vec!["b", "c"]);
        // The front resolves: the next queued request is promoted.
        s.apply_frame(resolved_frame(4, "a"));
        assert_eq!(front_id(&s), Some("b"));
        assert_eq!(queued_ids(&s), vec!["c"]);
        // A queued request resolves elsewhere: removed, front untouched.
        s.apply_frame(resolved_frame(5, "c"));
        assert_eq!(front_id(&s), Some("b"));
        assert!(queued_ids(&s).is_empty());
        s.apply_frame(resolved_frame(6, "b"));
        assert!(s.pending_approval.is_none());
        assert!(s.queued_approvals.is_empty());
    }

    #[test]
    fn duplicate_approval_request_ids_are_ignored() {
        let mut s = AppState::new();
        s.apply_frame(requested_frame(1, "a"));
        s.apply_frame(requested_frame(2, "a"));
        s.apply_frame(requested_frame(3, "b"));
        s.apply_frame(requested_frame(4, "b"));
        assert_eq!(front_id(&s), Some("a"));
        assert_eq!(queued_ids(&s), vec!["b"]);
    }

    #[test]
    fn resolving_an_unknown_approval_is_a_noop() {
        let mut s = AppState::new();
        s.apply_frame(requested_frame(1, "a"));
        s.apply_frame(requested_frame(2, "b"));
        s.apply_frame(resolved_frame(3, "zzz"));
        assert_eq!(front_id(&s), Some("a"));
        assert_eq!(queued_ids(&s), vec!["b"]);
    }

    #[test]
    fn turn_failure_and_abort_clear_every_approval() {
        let mut s = AppState::new();
        s.apply_frame(requested_frame(1, "a"));
        s.apply_frame(requested_frame(2, "b"));
        s.apply_frame(event_frame(
            3,
            Payload::TurnFailed {
                error: "boom".into(),
            },
        ));
        assert!(s.pending_approval.is_none());
        assert!(s.queued_approvals.is_empty());

        s.apply_frame(requested_frame(4, "c"));
        s.apply_frame(event_frame(5, Payload::TurnAborted {}));
        assert!(s.pending_approval.is_none());
        assert!(s.queued_approvals.is_empty());
    }

    #[test]
    fn snapshot_reset_drops_the_queue() {
        let mut s = AppState::new();
        s.apply_frame(requested_frame(1, "a"));
        s.apply_frame(requested_frame(2, "b"));
        s.apply_frame(snapshot_frame(Vec::new(), LiveState::default()));
        assert!(s.pending_approval.is_none());
        assert!(s.queued_approvals.is_empty());
    }

    #[test]
    fn disconnect_clears_approvals_and_in_flight_buffers() {
        let mut s = AppState::new();
        s.apply_frame(event_frame(
            1,
            Payload::Delta {
                delta: WireDelta::Text {
                    text: "partial".into(),
                },
            },
        ));
        s.apply_frame(requested_frame(2, "a"));
        s.apply_frame(requested_frame(3, "b"));
        s.running = true;
        s.on_disconnected();
        assert_eq!(s.conn, ConnState::Offline);
        assert!(!s.running);
        assert!(s.pending_approval.is_none());
        assert!(s.queued_approvals.is_empty());
        assert!(s.streaming_text.is_empty());
        assert!(s.displayed_streaming().is_empty());
    }

    #[test]
    fn truncated_args_get_an_ellipsis() {
        // The desktop consumed the shared modron_view::short (the local
        // byte-identical copy dropped, 2026-11-12 audit DRY fix); the
        // projection parity stays pinned here.
        assert_eq!(modron_view::short("abcde", 5), "abcde");
        assert_eq!(modron_view::short("abcdefgh", 4), "abcd…");
    }
}
