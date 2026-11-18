//! modron-view — shared client view-models (remote-control Phase 1).
//!
//! Every front end (TUI, desktop GUI, mobile app, server-side snapshots)
//! derives the same presentation from the same inputs through this crate,
//! so grouped tool activity and replayed transcripts render identically
//! everywhere. UI-agnostic by design: entries carry plain text a renderer
//! styles, never ratatui `Line`s.
//!
//! Two inputs feed it:
//! - live [`ToolActivity`] streams, folded by [`ActivityTracker`] into the
//!   grouped per-turn tool view (the canonical version of the TUI's
//!   `turn_tools`/`swarm` logic);
//! - persisted [`SessionEvent`] logs, replayed by [`replay`] into
//!   [`TranscriptEntry`] rows for late-joining clients (the store is the
//!   complete source of truth; live-only detail arrives separately as a
//!   protocol snapshot).

pub mod gen_ui;
pub mod slash;

/// The checkbox rows for a standing plan (shared copy: the TUI backfill,
/// the mobile shell, and the GUI render these verbatim).
pub fn plan_lines(items: &[modron_types::PlanItem]) -> Vec<String> {
    items
        .iter()
        .map(|item| {
            let mark = match item.status.as_str() {
                "done" => "\u{2713}",
                "active" => "\u{25b8}",
                "blocked" => "\u{2717}",
                _ => "\u{25cb}",
            };
            if item.done_criteria.is_empty() {
                format!("{mark} {}", item.text)
            } else {
                format!("{mark} {} - {}", item.text, item.done_criteria)
            }
        })
        .collect()
}

pub use gen_ui::{
    render_ascii, split_generative_ui, ChartKind, GenField, GenFieldKind, GenPoint, GenSegment,
    GenUi,
};

use modron_types::{AgentState, SessionEvent, ToolActivity, Usage};
use serde::{Deserialize, Serialize};

/// One grouped tool event for the collapsed tool view (lifted from the
/// TUI's `ToolEvent`; same fields, wire-safe).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolEvent {
    pub name: String,
    /// Output size in bytes (filled at Done).
    pub bytes: Option<u64>,
    pub is_error: bool,
    pub done: bool,
}

impl ToolEvent {
    /// The row as shown in an expanded panel, e.g. `✓ read — 2.1 KB` or
    /// `● bash …`.
    pub fn summary(&self) -> String {
        if !self.done {
            format!("● {} …", self.name)
        } else {
            let mark = if self.is_error { "✗" } else { "✓" };
            match self.bytes {
                Some(b) => format!("{mark} {} — {}", self.name, human_bytes(b)),
                None => format!("{mark} {}", self.name),
            }
        }
    }
}

/// `2.1 KB`-style byte sizes for tool panels and summaries.
/// The shared marker word on a private (`!!`) shell row. The row is shown
/// in every client but never sent to the model.
pub const PRIVATE_SHELL_MARK: &str = "private";

/// The shared row text of a shell entry: the `$ <command>` header, then one
/// row per output line. A private entry's header carries
/// [`PRIVATE_SHELL_MARK`]. Every client renders shell rows through this.
pub fn shell_rows(command: &str, lines: &[String], private: bool) -> Vec<String> {
    let header = if private {
        format!("$ {command} [{PRIVATE_SHELL_MARK}]")
    } else {
        format!("$ {command}")
    };
    std::iter::once(header)
        .chain(lines.iter().cloned())
        .collect()
}

pub fn human_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Compact token count for the context meter: exact below a thousand,
/// otherwise nearest thousand with a `k` suffix (`74k`, `180k`). Used by
/// every client's context label (`ctx 41% (74k/180k)`).
pub fn compact_tokens(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else {
        format!("{}k", n.saturating_add(500) / 1_000)
    }
}

/// The composer's placeholder, shown while its draft is empty.
pub const COMPOSER_PLACEHOLDER: &str = "Message";

/// The composer's Stop control label, shown while a turn runs on the web
/// host (AGENTS.md: a stop button stands in for the terminal's Esc).
pub const STOP_LABEL: &str = "stop";

/// The composer's Queue control label on the web host: queues the composer
/// text, as Alt+Enter does in the terminal.
pub const QUEUE_LABEL: &str = "queue";

/// The sandbox label shown in every client's settings views: `off`,
/// `seatbelt` (macOS Seatbelt) or `bwrap` (Linux bubblewrap).
pub fn sandbox_label(policy: modron_types::SandboxPolicy) -> &'static str {
    match policy {
        modron_types::SandboxPolicy::Off => "off",
        modron_types::SandboxPolicy::Seatbelt => "seatbelt",
        modron_types::SandboxPolicy::Bubblewrap => "bwrap",
    }
}

/// Truncate to `max_chars` with an ellipsis (the TUI's historical
/// `short()`; the desktop and CLI shipped byte-identical copies).
pub fn short(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max_chars).collect();
        format!("{cut}\u{2026}")
    }
}

/// Shown once per connection when a client receives a frame it cannot
/// decode (a newer server speaking to an older client). Shared so every
/// client says the same words.
pub const UNDECODABLE_FRAME_NOTICE: &str =
    "received a frame this client version cannot read - some updates may be missing; upgrade modron";

/// Character cap for the one-line approval summary. Every client calls
/// [`approval_summary`] with this value so the prompt reads the same.
pub const APPROVAL_SUMMARY_MAX: usize = 80;

/// Replace control characters and bidi overrides/isolates with `?` so a
/// tool call cannot hide or reorder text on an approval prompt.
pub fn sanitize_display(s: &str) -> String {
    s.chars()
        .map(|c| {
            let bidi = matches!(
                c,
                '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
            );
            if c.is_control() || bidi {
                '?'
            } else {
                c
            }
        })
        .collect()
}

/// The one-line approval prompt text (AGENTS.md D7): the essential
/// argument first (`bash` -> the command, `write`/`edit` -> the path,
/// anything else -> compact JSON), sanitized, capped at `max` chars. A
/// truncated summary ends with ` [+N chars]` where N is the hidden count.
pub fn approval_summary(name: &str, args: &serde_json::Value, max: usize) -> String {
    let essential = match name {
        "bash" => args.get("command").and_then(serde_json::Value::as_str),
        "write" | "edit" | "edit_lines" => args.get("path").and_then(serde_json::Value::as_str),
        _ => None,
    };
    let raw = match essential {
        Some(text) => text.to_string(),
        None => serde_json::to_string(args).unwrap_or_default(),
    };
    let clean = sanitize_display(&raw);
    let total = clean.chars().count();
    if total <= max {
        clean
    } else {
        let head: String = clean.chars().take(max).collect();
        format!("{head} [+{} chars]", total - max)
    }
}

/// Cache-hit percentage for usage: `100 * cached / input`, clamped to
/// `0..=100`. `Usage.input_tokens` already includes cache reads and writes
/// (E3), so `cached` is a subset of it and no addition is needed. `None`
/// when `input` is zero, so clients keep omitting the figure before any
/// tokens exist (the "absent before any tokens" contract).
pub fn cache_hit_pct(usage: &modron_types::Usage) -> Option<f64> {
    if usage.input_tokens == 0 {
        return None;
    }
    let pct = 100.0 * usage.cached_input_tokens as f64 / usage.input_tokens as f64;
    Some(pct.clamp(0.0, 100.0))
}

/// One subagent row of the swarm view (serde-safe: epoch millis instead
/// of `Instant`, so every client renders the same elapsed clock).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmRow {
    pub id: String,
    pub label: String,
    pub turns: u32,
    pub tokens: u64,
    pub last_tool: Option<String>,
    /// `None` = still running.
    pub done: Option<bool>,
    /// When the row appeared (epoch ms; elapsed-time rendering). Absent on
    /// rows rebuilt from the wire snapshot (the clock restarts there).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_ms: Option<u64>,
    /// Lifecycle state (PROTOCOL_V2 section 4). Rows from older peers have
    /// none and read as running; see [`SwarmRow::effective_state`].
    #[serde(default)]
    pub state: AgentState,
}

/// The state word a swarm row shows (shared by every client). `running`
/// and `done` keep their historic text.
pub fn agent_state_word(state: AgentState) -> &'static str {
    match state {
        AgentState::Running => "running",
        AgentState::WaitingApproval => "waiting approval",
        AgentState::Done => "done",
        AgentState::Failed => "failed",
        AgentState::Killed => "killed",
    }
}

/// The Notify / toast / last-note text for a finished background sub-agent
/// (PROTOCOL_V2 section 4): `sub-agent <label> finished` or `... failed`.
pub fn agent_finished_text(label: &str, ok: bool) -> String {
    let verdict = if ok { "finished" } else { "failed" };
    format!("sub-agent {label} {verdict}")
}

/// Wall-clock epoch milliseconds (elapsed-time rendering without
/// `Instant`, which is not serde-safe).
pub fn now_epoch_ms() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

impl SwarmRow {
    /// The state the row shows. A row from an older peer carries no state,
    /// so its `done` flag decides between done and failed.
    pub fn effective_state(&self) -> AgentState {
        match (self.state, self.done) {
            (AgentState::Running, Some(true)) => AgentState::Done,
            (AgentState::Running, Some(false)) => AgentState::Failed,
            (state, _) => state,
        }
    }

    /// The display row, e.g. `⏵ [reviewer] done · 3 turns · 1.2k tok ·
    /// last: read`.
    pub fn summary(&self) -> String {
        format!(
            "⏵ [{}] {} · {} turns · {} tok · last: {}",
            self.label,
            agent_state_word(self.effective_state()),
            self.turns,
            self.tokens,
            self.last_tool.as_deref().unwrap_or("-")
        )
    }
}

/// Folds a live [`ToolActivity`] stream into the grouped per-turn tool
/// view: `turn_tools` is the expandable panel's content, `subagents` the
/// live swarm rows. Servers embed this behind the fan-out hub so every
/// attached client's grouped view matches the TUI's exactly.
#[derive(Clone, Debug, Default)]
pub struct ActivityTracker {
    pub turn_tools: Vec<ToolEvent>,
    pub subagents: Vec<SwarmRow>,
}

impl ActivityTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one tool-activity event (canonical Start/Done grouping and
    /// swarm-row updates).
    pub fn on_tool(&mut self, activity: &ToolActivity) {
        match activity {
            ToolActivity::Start { name, .. } => {
                self.turn_tools.push(ToolEvent {
                    name: name.clone(),
                    bytes: None,
                    is_error: false,
                    done: false,
                });
            }
            ToolActivity::Done {
                name,
                is_error,
                bytes,
                ..
            } => {
                if let Some(event) = self
                    .turn_tools
                    .iter_mut()
                    .rev()
                    .find(|e| e.name.as_str() == name.as_str() && !e.done)
                {
                    event.done = true;
                    event.is_error = *is_error;
                    event.bytes = Some(*bytes as u64);
                } else {
                    self.turn_tools.push(ToolEvent {
                        name: name.clone(),
                        bytes: Some(*bytes as u64),
                        is_error: *is_error,
                        done: true,
                    });
                }
            }
            ToolActivity::SubagentStarted { id, label } => {
                self.subagents.push(SwarmRow {
                    id: id.clone(),
                    label: label.clone(),
                    turns: 0,
                    tokens: 0,
                    last_tool: None,
                    done: None,
                    started_ms: now_epoch_ms(),
                    state: AgentState::Running,
                });
            }
            ToolActivity::SubagentProgress {
                id,
                turns,
                tokens,
                last_tool,
                ..
            } => {
                if let Some(row) = self.subagents.iter_mut().find(|r| r.id == *id) {
                    row.turns = row.turns.max(*turns);
                    row.tokens = row.tokens.max(*tokens);
                    row.last_tool = last_tool.clone();
                }
            }
            ToolActivity::SubagentFinished {
                id,
                tokens,
                turns,
                is_error,
                ..
            } => {
                if let Some(row) = self.subagents.iter_mut().find(|r| r.id == *id) {
                    row.turns = row.turns.max(*turns);
                    row.tokens = row.tokens.max(*tokens);
                    row.done = Some(!*is_error);
                    // A kill is sticky: its finish keeps the killed state.
                    if row.state != AgentState::Killed {
                        row.state = if *is_error {
                            AgentState::Failed
                        } else {
                            AgentState::Done
                        };
                    }
                }
            }
            ToolActivity::SubagentState { id, state, .. } => {
                if let Some(row) = self.subagents.iter_mut().find(|r| r.id == *id) {
                    // Only a live row changes state; a settled one keeps its
                    // finish.
                    if row.done.is_none() {
                        row.state = *state;
                        if *state == AgentState::Killed {
                            row.done = Some(false);
                        }
                    }
                }
            }
            // Background settlement is a push (AgentFinished), not a row change.
            ToolActivity::BackgroundFinished { .. } => {}
        }
    }

    /// The live strip payload: latest event summary + in-flight count.
    pub fn live_strip(&self) -> Option<(String, usize)> {
        let latest = self.turn_tools.last()?.summary();
        Some((latest, self.turn_tools.len()))
    }

    /// `⏺ 21 tools · glob ×9 · read ×8 …` — counts, biggest first.
    pub fn turn_summary_line(&self) -> Option<String> {
        if self.turn_tools.is_empty() {
            return None;
        }
        Some(tool_summary_line(&self.turn_tools))
    }

    /// The committed swarm block (one header + one row per agent), then
    /// clear. Empty when no agent ran.
    pub fn take_swarm_summary(&mut self) -> Vec<String> {
        if self.subagents.is_empty() {
            return Vec::new();
        }
        let mut lines = vec![format!(
            "⎇ swarm: {} agent(s), {} tok total",
            self.subagents.len(),
            self.subagents.iter().map(|r| r.tokens).sum::<u64>()
        )];
        lines.extend(self.subagents.iter().map(SwarmRow::summary));
        self.subagents.clear();
        lines
    }

    /// Reset the per-turn view (turn ended; events already committed).
    pub fn clear_turn(&mut self) {
        self.turn_tools.clear();
    }
}

/// `⏺ 21 tools · glob ×9 · read ×8 …` — counts, biggest first (canonical
/// version of the TUI's committed per-turn summary line).
pub fn tool_summary_line(events: &[ToolEvent]) -> String {
    let mut counts: std::collections::BTreeMap<&str, usize> = Default::default();
    for event in events {
        *counts.entry(event.name.as_str()).or_default() += 1;
    }
    let mut parts: Vec<(usize, &str)> = counts.into_iter().map(|(n, c)| (c, n)).collect();
    parts.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
    let total: usize = parts.iter().map(|(c, _)| *c).sum();
    if total == 1 {
        return format!("⏺ 1 tool · {}", parts[0].1);
    }
    let shown: Vec<String> = parts
        .iter()
        .take(4)
        .map(|(c, n)| {
            if *c > 1 {
                format!("{n} ×{c}")
            } else {
                n.to_string()
            }
        })
        .collect();
    let mut line = format!("⏺ {total} tools · {}", shown.join(" · "));
    if parts.len() > 4 {
        line.push_str(" · …");
    }
    line
}

/// One replayed transcript row (renderer-agnostic: plain text + semantics).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TranscriptEntry {
    /// The user's task text (the `❯ echo` row).
    Task { text: String },
    /// A user message that is not the task echo.
    User { text: String },
    /// Assistant output text.
    Assistant { text: String },
    /// A turn's grouped tool activity: the collapsed summary line plus the
    /// events behind the expandable panel.
    ToolSummary {
        line: String,
        events: Vec<ToolEvent>,
    },
    /// A committed subagent-swarm block.
    Swarm { lines: Vec<String> },
    /// A `!` / `!!` shell command the user ran (not a model tool call).
    /// `lines` are the output lines; `private` marks a `!!` command that
    /// never reaches the model ([`shell_rows`] is the shared row text).
    Shell {
        command: String,
        lines: Vec<String>,
        is_error: bool,
        private: bool,
    },
    /// Model-selection note.
    Model { model: String, reason: String },
    /// Usage accounting for one turn.
    Usage { model: String, usage: Usage },
    /// Compaction note.
    Compaction {
        summary: String,
        summarized_events: u32,
    },
    /// The standing plan (measurable subtasks; every write replaces the
    /// whole plan). UIs render checkbox rows; the latest plan is the
    /// run's tracked state.
    Plan { items: Vec<modron_types::PlanItem> },
    /// A generative-UI widget parsed from a ```modron-ui fenced block in
    /// an assistant answer (table, chart, form). The GUI renders native
    /// widgets; the TUI renders the ASCII projection.
    GenerativeUi { ui: crate::gen_ui::GenUi },
    /// An independent reviewer verdict (H4a). `summary` is the one-line
    /// verdict ([`review_summary`]); `findings` are the expandable rows
    /// ([`finding_line`]).
    Review {
        verdict: modron_types::ReviewVerdict,
        summary: String,
        findings: Vec<String>,
    },
    /// A one-line note that is not a model turn: the session was handed to
    /// another host ([`handoff_note`]).
    Note { text: String },
}

/// The shared wording stem for a handed-off session (C6c). Every client shows
/// the note built by [`handoff_note`], never its own words.
pub const HANDOFF_NOTE: &str = "session handed off to another host";

/// The handoff transcript note: `session handed off to another host (ref)`,
/// or `... to <host> (ref)` when the export named the target.
pub fn handoff_note(to: Option<&str>, carrier_ref: &str) -> String {
    match to {
        Some(host) => format!("{HANDOFF_NOTE} to {host} ({carrier_ref})"),
        None => format!("{HANDOFF_NOTE} ({carrier_ref})"),
    }
}

/// The one-line reviewer verdict shared by every client: `review: pass`,
/// `review: skipped`, or `review: needs fix - N findings` (`1 finding`
/// singular).
pub fn review_summary(verdict: modron_types::ReviewVerdict, finding_count: usize) -> String {
    use modron_types::ReviewVerdict;
    match verdict {
        ReviewVerdict::Pass => "review: pass".to_string(),
        ReviewVerdict::Skipped => "review: skipped".to_string(),
        ReviewVerdict::NeedsFix => {
            let noun = if finding_count == 1 {
                "finding"
            } else {
                "findings"
            };
            format!("review: needs fix - {finding_count} {noun}")
        }
    }
}

/// One reviewer finding row: `p1 path:line - text`. The `path:line` part is
/// omitted when absent (`p1 - text`); the line alone is never shown without
/// its path.
pub fn finding_line(finding: &modron_types::Finding) -> String {
    let code = finding.severity.code().to_ascii_lowercase();
    let location = match (&finding.path, finding.line) {
        (Some(path), Some(line)) => format!("{path}:{line} - "),
        (Some(path), None) => format!("{path} - "),
        (None, _) => "- ".to_string(),
    };
    format!("{code} {location}{}", finding.text)
}

/// Rebuild the display transcript from a session's persisted event log —
/// what a late-joining client renders before live events resume. Live-only
/// detail (in-flight streaming, the current tool strip, pending approvals)
/// is NOT here; it arrives as a protocol snapshot alongside this replay.
///
/// Tool names are recovered from the preceding assistant message's
/// `tool_calls` (`ToolResult` events carry only `call_id`); unrecoverable
/// calls render as `tool`. `TurnTrace`/`Checkpoint` are observability and
/// produce no display rows, matching the TUI.
/// Incremental replay: the exact fold of [`replay`], one event at a time.
/// GUI clients hold one of these and feed it live events so a streamed
/// frame never re-replays the whole log (the desktop pins folder == full
/// replay with a parity test).
#[derive(Debug, Default)]
pub struct ReplayFolder {
    entries: Vec<TranscriptEntry>,
    // call_id -> tool name from the most recent assistant message.
    call_names: std::collections::HashMap<String, String>,
    turn_tools: Vec<ToolEvent>,
}

impl ReplayFolder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop all folded state (snapshot resets, session switches).
    pub fn reset(&mut self) {
        self.entries.clear();
        self.call_names.clear();
        self.turn_tools.clear();
    }

    /// Fold one event; returns the entries it produced (zero or more).
    /// A run of `ToolResult` events accumulates and only lands as ONE
    /// `ToolSummary` row when a boundary event arrives (or on
    /// `into_entries`) - exactly like full-log `replay`, so a live fold
    /// and a re-attach replay always agree.
    pub fn push(&mut self, event: &SessionEvent) -> &[TranscriptEntry] {
        let start = self.entries.len();
        self.fold_event(event);
        &self.entries[start..]
    }

    /// Fold one event into the accumulator, then flush any tools the event
    /// boundary closes. `push` returns exactly what this appended.
    fn fold_event(&mut self, event: &SessionEvent) {
        match event {
            SessionEvent::Task { text } => {
                self.flush_tools();
                self.entries
                    .push(TranscriptEntry::Task { text: text.clone() });
            }
            SessionEvent::UserShell {
                command,
                output,
                is_error,
                private,
            } => {
                self.flush_tools();
                self.entries.push(TranscriptEntry::Shell {
                    command: command.clone(),
                    lines: output.lines().map(str::to_string).collect(),
                    is_error: *is_error,
                    private: *private,
                });
            }
            SessionEvent::Plan { items } => {
                self.flush_tools();
                self.entries.push(TranscriptEntry::Plan {
                    items: items.clone(),
                });
            }
            SessionEvent::Message { message, .. } => match message.role {
                modron_types::Role::User => {
                    self.flush_tools();
                    self.entries.push(TranscriptEntry::User {
                        text: message.content.clone(),
                    });
                }
                modron_types::Role::Assistant => {
                    self.flush_tools();
                    for call in &message.tool_calls {
                        self.call_names.insert(call.id.clone(), call.name.clone());
                    }
                    if !message.content.is_empty() {
                        // Generative-UI split: ```modron-ui blocks become
                        // native-widget entries; the surrounding prose
                        // stays as assistant text.
                        for segment in gen_ui::split_generative_ui(&message.content) {
                            match segment {
                                gen_ui::GenSegment::Text(text) => {
                                    self.entries.push(TranscriptEntry::Assistant { text });
                                }
                                gen_ui::GenSegment::Ui(ui) => {
                                    self.entries.push(TranscriptEntry::GenerativeUi { ui });
                                }
                            }
                        }
                    }
                }
                modron_types::Role::Tool => {
                    // Tool results ride the tool summary via their
                    // companion ToolResult events, not as rows.
                }
            },
            SessionEvent::ToolResult { call_id, outcome } => {
                let name = self
                    .call_names
                    .get(call_id)
                    .cloned()
                    .unwrap_or_else(|| "tool".to_string());
                self.turn_tools.push(ToolEvent {
                    name,
                    bytes: Some(outcome.content.len() as u64),
                    is_error: outcome.is_error,
                    done: true,
                });
            }
            SessionEvent::ModelSelected { model, reason, .. } => {
                self.flush_tools();
                self.entries.push(TranscriptEntry::Model {
                    model: model.clone(),
                    reason: reason.clone(),
                });
            }
            SessionEvent::Usage { model, usage } => {
                self.flush_tools();
                self.entries.push(TranscriptEntry::Usage {
                    model: model.clone(),
                    usage: *usage,
                });
            }
            SessionEvent::Compaction {
                summary,
                summarized_events,
            } => {
                self.flush_tools();
                self.entries.push(TranscriptEntry::Compaction {
                    summary: summary.clone(),
                    summarized_events: *summarized_events,
                });
            }
            SessionEvent::Review {
                verdict, findings, ..
            } => {
                self.flush_tools();
                self.entries.push(TranscriptEntry::Review {
                    verdict: *verdict,
                    summary: review_summary(*verdict, findings.len()),
                    findings: findings.iter().map(finding_line).collect(),
                });
            }
            SessionEvent::HandedOff {
                to, carrier_ref, ..
            } => {
                self.flush_tools();
                self.entries.push(TranscriptEntry::Note {
                    text: handoff_note(to.as_deref(), carrier_ref),
                });
            }
            // A team stage note: one transcript note, shown as written.
            SessionEvent::Progress { text } => {
                self.flush_tools();
                self.entries
                    .push(TranscriptEntry::Note { text: text.clone() });
            }
            SessionEvent::TurnTrace { .. } | SessionEvent::Checkpoint { .. } => {}
        }
    }

    /// Flush accumulated tool events into one summary row when the event
    /// boundary closes the run of tool activity.
    fn flush_tools(&mut self) {
        if self.turn_tools.is_empty() {
            return;
        }
        let line = tool_summary_line(&self.turn_tools);
        self.entries.push(TranscriptEntry::ToolSummary {
            line,
            events: std::mem::take(&mut self.turn_tools),
        });
    }

    pub fn entries(&self) -> &[TranscriptEntry] {
        &self.entries
    }

    pub fn into_entries(mut self) -> Vec<TranscriptEntry> {
        // Same terminal flush as replay's final line.
        if !self.turn_tools.is_empty() {
            self.flush_tools();
        }
        self.entries
    }
}

/// Full-log replay into display entries. This is the ONLY place the
/// display-level transcript fold lives (repo law): grouping (tool runs ->
/// one summary row), assistant-message generative-UI splitting, and the
/// `TranscriptEntry` shapes all come from here, and every client replays
/// the same way.
///
/// (moved doc: see [`ReplayFolder`] for the incremental form)
pub fn replay(events: &[SessionEvent]) -> Vec<TranscriptEntry> {
    let mut folder = ReplayFolder::new();
    for event in events {
        folder.push(event);
    }
    folder.into_entries()
}

/// Sum every `Usage` event in a log (the cumulative token total a resumed
/// client should show).
pub fn usage_totals(events: &[SessionEvent]) -> Usage {
    let mut total = Usage::default();
    for event in events {
        if let SessionEvent::Usage { usage, .. } = event {
            total.input_tokens += usage.input_tokens;
            total.output_tokens += usage.output_tokens;
            total.cached_input_tokens += usage.cached_input_tokens;
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use modron_types::{Message, ToolCall, ToolOutcome};
    use serde_json::json;

    #[test]
    fn compact_tokens_is_exact_below_a_thousand_and_rounds_to_k() {
        assert_eq!(compact_tokens(0), "0");
        assert_eq!(compact_tokens(999), "999");
        assert_eq!(compact_tokens(1_000), "1k");
        assert_eq!(compact_tokens(1_499), "1k");
        assert_eq!(compact_tokens(1_500), "2k");
        assert_eq!(compact_tokens(74_000), "74k");
        assert_eq!(compact_tokens(180_000), "180k");
    }

    #[test]
    fn shared_control_labels_are_the_web_copy() {
        assert_eq!(STOP_LABEL, "stop");
        assert_eq!(QUEUE_LABEL, "queue");
    }

    #[test]
    fn cache_hit_pct_is_cached_over_input() {
        let usage = modron_types::Usage {
            input_tokens: 1000,
            output_tokens: 10,
            cached_input_tokens: 250,
        };
        assert_eq!(cache_hit_pct(&usage), Some(25.0));
    }

    #[test]
    fn cache_hit_pct_is_absent_without_input_and_clamped() {
        let empty = modron_types::Usage::default();
        assert_eq!(cache_hit_pct(&empty), None);
        // Corrupt data (cached > input) must not report more than 100%.
        let corrupt = modron_types::Usage {
            input_tokens: 10,
            output_tokens: 0,
            cached_input_tokens: 40,
        };
        assert_eq!(cache_hit_pct(&corrupt), Some(100.0));
    }

    fn done(name: &str, bytes: u64, is_error: bool) -> ToolActivity {
        ToolActivity::Done {
            call_id: None,
            name: name.to_string(),
            is_error,
            bytes: bytes as usize,
        }
    }

    #[test]
    fn groups_start_then_done_in_place() {
        let mut t = ActivityTracker::new();
        t.on_tool(&ToolActivity::Start {
            call_id: None,
            name: "read".into(),
        });
        t.on_tool(&ToolActivity::Start {
            call_id: None,
            name: "bash".into(),
        });
        t.on_tool(&done("read", 2150, false));
        assert_eq!(t.turn_tools.len(), 2);
        assert_eq!(t.turn_tools[0].summary(), "✓ read — 2.1 KB");
        assert_eq!(t.turn_tools[1].summary(), "● bash …");
        assert_eq!(
            t.live_strip(),
            Some(("● bash …".to_string(), 2)),
            "strip shows the latest event"
        );
    }

    #[test]
    fn done_without_start_is_still_shown() {
        let mut t = ActivityTracker::new();
        t.on_tool(&done("grep", 10, false));
        assert_eq!(t.turn_tools.len(), 1);
        assert!(t.turn_tools[0].done);
    }

    #[test]
    fn summary_line_counts_and_orders() {
        let mut t = ActivityTracker::new();
        for _ in 0..9 {
            t.on_tool(&ToolActivity::Start {
                call_id: None,
                name: "glob".into(),
            });
            t.on_tool(&done("glob", 1, false));
        }
        for _ in 0..8 {
            t.on_tool(&ToolActivity::Start {
                call_id: None,
                name: "read".into(),
            });
            t.on_tool(&done("read", 1, false));
        }
        let line = t.turn_summary_line().unwrap();
        assert_eq!(line, "⏺ 17 tools · glob ×9 · read ×8");
    }

    #[test]
    fn summary_line_singular_and_overflow() {
        let single = tool_summary_line(&[ToolEvent {
            name: "read".into(),
            bytes: Some(1),
            is_error: false,
            done: true,
        }]);
        assert_eq!(single, "⏺ 1 tool · read");

        let mut events: Vec<ToolEvent> = (0..6)
            .map(|i| ToolEvent {
                name: format!("t{i}"),
                bytes: Some(1),
                is_error: false,
                done: true,
            })
            .collect();
        events.push(ToolEvent {
            name: "t0".into(),
            bytes: Some(1),
            is_error: false,
            done: true,
        });
        let line = tool_summary_line(&events);
        assert!(
            line.ends_with(" · …"),
            "more than 4 names appends an ellipsis"
        );
        assert!(line.starts_with("⏺ 7 tools · t0 ×2"));
    }

    #[test]
    fn swarm_rows_track_lifecycle() {
        let mut t = ActivityTracker::new();
        t.on_tool(&ToolActivity::SubagentStarted {
            id: "a1".into(),
            label: "explore".into(),
        });
        t.on_tool(&ToolActivity::SubagentProgress {
            id: "a1".into(),
            label: "explore".into(),
            turns: 2,
            tokens: 500,
            last_tool: Some("grep".into()),
        });
        t.on_tool(&ToolActivity::SubagentFinished {
            id: "a1".into(),
            label: "explore".into(),
            tokens: 700,
            turns: 3,
            is_error: false,
        });
        assert_eq!(t.subagents.len(), 1);
        assert_eq!(t.subagents[0].done, Some(true));
        assert_eq!(t.subagents[0].tokens, 700);
        assert_eq!(t.subagents[0].turns, 3);
        let lines = t.take_swarm_summary();
        assert_eq!(lines.len(), 2, "header + one row");
        assert!(lines[0].starts_with("⎇ swarm: 1 agent(s), 700 tok"));
        assert!(t.take_swarm_summary().is_empty(), "cleared after commit");
    }

    #[test]
    fn swarm_rows_show_waiting_approval_and_killed_states() {
        let mut t = ActivityTracker::new();
        t.on_tool(&ToolActivity::SubagentStarted {
            id: "k1".into(),
            label: "delegate".into(),
        });
        t.on_tool(&ToolActivity::SubagentState {
            id: "k1".into(),
            label: "delegate".into(),
            state: AgentState::WaitingApproval,
        });
        assert!(t.subagents[0].summary().contains("] waiting approval "));
        t.on_tool(&ToolActivity::SubagentState {
            id: "k1".into(),
            label: "delegate".into(),
            state: AgentState::Running,
        });
        assert_eq!(t.subagents[0].state, AgentState::Running, "the ask settled");
        assert!(t.subagents[0].summary().contains("] running "));

        t.on_tool(&ToolActivity::SubagentState {
            id: "k1".into(),
            label: "delegate".into(),
            state: AgentState::Killed,
        });
        t.on_tool(&ToolActivity::SubagentFinished {
            id: "k1".into(),
            label: "delegate".into(),
            tokens: 1_050,
            turns: 1,
            is_error: true,
        });
        assert_eq!(t.subagents[0].state, AgentState::Killed, "a kill is sticky");
        assert!(t.subagents[0].summary().contains("] killed "));

        t.on_tool(&ToolActivity::SubagentStarted {
            id: "f1".into(),
            label: "persona".into(),
        });
        t.on_tool(&ToolActivity::SubagentFinished {
            id: "f1".into(),
            label: "persona".into(),
            tokens: 0,
            turns: 0,
            is_error: true,
        });
        assert!(t.subagents[1].summary().contains("] failed "));
    }

    #[test]
    fn background_settlement_is_not_a_row_change() {
        let mut t = ActivityTracker::new();
        t.on_tool(&ToolActivity::SubagentStarted {
            id: "b1".into(),
            label: "delegate".into(),
        });
        t.on_tool(&ToolActivity::BackgroundFinished {
            id: "b1".into(),
            label: "delegate".into(),
            ok: true,
            brief: "done".into(),
        });
        assert_eq!(t.subagents[0].done, None, "the push is not the finish");
        assert_eq!(t.subagents[0].state, AgentState::Running);
    }

    #[test]
    fn rows_from_older_peers_read_their_done_flag_and_shared_words_hold() {
        let row = SwarmRow {
            id: "o".into(),
            label: "explore".into(),
            turns: 2,
            tokens: 9,
            last_tool: None,
            done: Some(true),
            started_ms: None,
            state: AgentState::Running,
        };
        assert!(row.summary().contains("] done "));
        let failed = SwarmRow {
            done: Some(false),
            ..row.clone()
        };
        assert!(failed.summary().contains("] failed "));
        assert_eq!(
            agent_state_word(AgentState::WaitingApproval),
            "waiting approval"
        );
        assert_eq!(
            agent_finished_text("delegate", true),
            "sub-agent delegate finished"
        );
        assert_eq!(
            agent_finished_text("persona", false),
            "sub-agent persona failed"
        );
    }

    fn fixture_events() -> Vec<SessionEvent> {
        let tool_call = ToolCall {
            id: "call-1".into(),
            name: "read".into(),
            arguments: serde_json::json!({"path": "a.rs"}),
        };
        let mut assistant = Message::assistant("looking at the file");
        assistant.tool_calls = vec![tool_call];
        vec![
            SessionEvent::Task {
                text: "read a.rs".into(),
            },
            SessionEvent::Message {
                parent_id: None,
                message: assistant,
            },
            SessionEvent::ToolResult {
                call_id: "call-1".into(),
                outcome: ToolOutcome::ok("file contents here"),
            },
            SessionEvent::Message {
                parent_id: None,
                message: Message::assistant("it says hi"),
            },
            SessionEvent::ModelSelected {
                model: "m1".into(),
                reason: "default".into(),
                action: None,
                context_tokens: None,
            },
            SessionEvent::Usage {
                model: "m1".into(),
                usage: Usage {
                    input_tokens: 100,
                    output_tokens: 10,
                    cached_input_tokens: 40,
                },
            },
            SessionEvent::TurnTrace {
                turn: 1,
                harness_version: "test".into(),
                provider: "mock".into(),
                model: "m1".into(),
                prompt_hash: None,
                latency_ms: None,
                cost_usd: None,
                usage: None,
                verifier: None,
                failure_label: None,
            },
        ]
    }

    #[test]
    fn replay_builds_display_entries_from_a_log() {
        let entries = replay(&fixture_events());
        let kinds: Vec<&str> = entries
            .iter()
            .map(|e| match e {
                TranscriptEntry::Task { .. } => "task",
                TranscriptEntry::User { .. } => "user",
                TranscriptEntry::Assistant { .. } => "assistant",
                TranscriptEntry::ToolSummary { .. } => "tools",
                TranscriptEntry::Swarm { .. } => "swarm",
                TranscriptEntry::Shell { .. } => "shell",
                TranscriptEntry::Model { .. } => "model",
                TranscriptEntry::Usage { .. } => "usage",
                TranscriptEntry::Compaction { .. } => "compaction",
                TranscriptEntry::GenerativeUi { .. } => "gen_ui",
                TranscriptEntry::Plan { .. } => "plan",
                TranscriptEntry::Review { .. } => "review",
                TranscriptEntry::Note { .. } => "note",
            })
            .collect();
        assert_eq!(
            kinds,
            vec!["task", "assistant", "tools", "assistant", "model", "usage"],
            "TurnTrace is skipped; tool results collapse into one summary"
        );
        match &entries[2] {
            TranscriptEntry::ToolSummary { line, events } => {
                assert_eq!(line, "⏺ 1 tool · read");
                assert_eq!(events.len(), 1);
                assert_eq!(events[0].name, "read", "name recovered from tool_calls");
                assert_eq!(events[0].bytes, Some("file contents here".len() as u64));
            }
            other => panic!("expected tool summary, got {other:?}"),
        }
    }

    #[test]
    fn replay_survives_unresolved_call_ids() {
        let events = vec![SessionEvent::ToolResult {
            call_id: "ghost".into(),
            outcome: ToolOutcome::err("nope"),
        }];
        let entries = replay(&events);
        match &entries[0] {
            TranscriptEntry::ToolSummary { events, .. } => {
                assert_eq!(events[0].name, "tool");
                assert!(events[0].is_error);
            }
            other => panic!("expected tool summary, got {other:?}"),
        }
    }

    #[test]
    fn usage_totals_sums_usage_events() {
        let total = usage_totals(&fixture_events());
        assert_eq!(total.input_tokens, 100);
        assert_eq!(total.output_tokens, 10);
        assert_eq!(total.cached_input_tokens, 40);
    }

    #[test]
    fn view_types_roundtrip_json() {
        let entry = TranscriptEntry::ToolSummary {
            line: "⏺ 1 tool · read".into(),
            events: vec![ToolEvent {
                name: "read".into(),
                bytes: Some(10),
                is_error: false,
                done: true,
            }],
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"kind\":\"tool_summary\""));
        let back: TranscriptEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back, entry);
    }

    #[test]
    fn approval_summary_leads_with_the_essential_argument() {
        // bash: the command itself, not a JSON wrapper around it.
        let s = approval_summary("bash", &json!({"command": "cargo test"}), 80);
        assert_eq!(s, "cargo test");
        // write / edit: the path.
        for tool in ["write", "edit"] {
            let s = approval_summary(tool, &json!({"path": "src/a.rs", "content": "x"}), 80);
            assert!(s.starts_with("src/a.rs"), "{tool}: {s}");
        }
        // Anything else: compact JSON of the whole call.
        let s = approval_summary("grep", &json!({"pattern": "todo"}), 80);
        assert_eq!(s, r#"{"pattern":"todo"}"#);
        // A bash call with no string command falls back to compact JSON.
        let s = approval_summary("bash", &json!({"cmd": 3}), 80);
        assert_eq!(s, r#"{"cmd":3}"#);
    }

    #[test]
    fn approval_summary_marks_truncation_with_the_hidden_count() {
        let long = "x".repeat(100);
        let s = approval_summary("bash", &json!({ "command": long }), 20);
        assert_eq!(s, format!("{} [+80 chars]", "x".repeat(20)));
        // Exactly at the cap is not truncated: no marker.
        let fits = "y".repeat(20);
        assert_eq!(
            approval_summary("bash", &json!({ "command": fits.clone() }), 20),
            fits
        );
        // The marker counts chars, not bytes.
        let multi = "\u{e9}".repeat(30);
        let s = approval_summary("write", &json!({ "path": multi }), 10);
        assert!(s.ends_with(" [+20 chars]"), "{s}");
    }

    #[test]
    fn approval_summary_replaces_control_and_bidi_characters() {
        let s = approval_summary("bash", &json!({"command": "ls\u{1b}[2J\u{202e}rm"}), 80);
        assert_eq!(s, "ls?[2J?rm");
        assert!(!s.contains('\u{1b}') && !s.contains('\u{202e}'));
        let s = approval_summary("write", &json!({"path": "a\u{2066}b\nc"}), 80);
        assert_eq!(s, "a?b?c");
    }

    #[test]
    fn approval_max_is_the_one_shared_cap() {
        assert_eq!(APPROVAL_SUMMARY_MAX, 80);
    }

    #[test]
    fn sandbox_label_words_are_shared() {
        use modron_types::SandboxPolicy;
        assert_eq!(sandbox_label(SandboxPolicy::Off), "off");
        assert_eq!(sandbox_label(SandboxPolicy::Seatbelt), "seatbelt");
        assert_eq!(sandbox_label(SandboxPolicy::Bubblewrap), "bwrap");
    }

    fn finding(
        severity: modron_types::Severity,
        path: Option<&str>,
        line: Option<u32>,
        text: &str,
    ) -> modron_types::Finding {
        modron_types::Finding {
            severity,
            path: path.map(String::from),
            line,
            text: text.into(),
        }
    }

    #[test]
    fn review_summary_has_three_shapes() {
        use modron_types::ReviewVerdict;
        assert_eq!(review_summary(ReviewVerdict::Pass, 0), "review: pass");
        assert_eq!(review_summary(ReviewVerdict::Skipped, 0), "review: skipped");
        assert_eq!(
            review_summary(ReviewVerdict::NeedsFix, 1),
            "review: needs fix - 1 finding"
        );
        assert_eq!(
            review_summary(ReviewVerdict::NeedsFix, 3),
            "review: needs fix - 3 findings"
        );
    }

    #[test]
    fn finding_line_omits_absent_location() {
        use modron_types::Severity;
        assert_eq!(
            finding_line(&finding(
                Severity::P1,
                Some("src/a.rs"),
                Some(7),
                "off by one"
            )),
            "p1 src/a.rs:7 - off by one"
        );
        assert_eq!(
            finding_line(&finding(Severity::P2, Some("src/a.rs"), None, "no line")),
            "p2 src/a.rs - no line"
        );
        assert_eq!(
            finding_line(&finding(Severity::P0, None, None, "general concern")),
            "p0 - general concern"
        );
    }

    #[test]
    fn replay_folds_review_events_in_log_order() {
        use modron_types::{ReviewVerdict, Severity};
        let events = vec![
            SessionEvent::Message {
                parent_id: None,
                message: Message::assistant("done"),
            },
            SessionEvent::Review {
                verdict: ReviewVerdict::NeedsFix,
                findings: vec![finding(Severity::P1, Some("a.rs"), Some(2), "bad")],
                model: "rev".into(),
            },
            SessionEvent::Review {
                verdict: ReviewVerdict::Pass,
                findings: Vec::new(),
                model: "rev".into(),
            },
            SessionEvent::Review {
                verdict: ReviewVerdict::Skipped,
                findings: Vec::new(),
                model: "rev".into(),
            },
        ];
        let entries = replay(&events);
        let reviews: Vec<(&str, &str, Vec<String>)> = entries
            .iter()
            .filter_map(|entry| match entry {
                TranscriptEntry::Review {
                    verdict,
                    summary,
                    findings,
                } => Some((
                    match verdict {
                        ReviewVerdict::Pass => "pass",
                        ReviewVerdict::NeedsFix => "needs_fix",
                        ReviewVerdict::Skipped => "skipped",
                    },
                    summary.as_str(),
                    findings.clone(),
                )),
                _ => None,
            })
            .collect();
        assert_eq!(
            reviews,
            vec![
                (
                    "needs_fix",
                    "review: needs fix - 1 finding",
                    vec!["p1 a.rs:2 - bad".to_string()]
                ),
                ("pass", "review: pass", Vec::new()),
                ("skipped", "review: skipped", Vec::new()),
            ]
        );
        assert!(matches!(entries[0], TranscriptEntry::Assistant { .. }));
        assert_eq!(
            entries.len(),
            4,
            "one row per Review event after the message"
        );
    }
}

#[cfg(test)]
mod shell_tests {
    use super::*;

    #[test]
    fn shell_rows_show_header_then_output_with_the_private_marker() {
        let lines = vec!["hello".to_string(), "exit code: 0".to_string()];
        assert_eq!(
            shell_rows("echo hello", &lines, false),
            vec!["$ echo hello", "hello", "exit code: 0"]
        );
        assert_eq!(
            shell_rows("echo hello", &lines, true),
            vec!["$ echo hello [private]", "hello", "exit code: 0"]
        );
        assert_eq!(shell_rows("true", &[], false), vec!["$ true"]);
    }

    #[test]
    fn replay_folds_a_handed_off_fact_into_one_note() {
        let events = vec![
            SessionEvent::Task { text: "t".into() },
            SessionEvent::HandedOff {
                to: None,
                at: chrono::Utc::now(),
                carrier_ref: "refs/heads/modron/handoff/x".into(),
            },
        ];
        let entries = replay(&events);
        assert_eq!(
            entries.last(),
            Some(&TranscriptEntry::Note {
                text: "session handed off to another host (refs/heads/modron/handoff/x)".into(),
            })
        );
        // Team stage notes replay as transcript notes, in log order, and
        // flush no tool rows of their own.
        let events = vec![
            SessionEvent::Task { text: "t".into() },
            SessionEvent::Progress {
                text: "team plan: 2 item(s)".into(),
            },
            SessionEvent::Progress {
                text: "round 1 started: 2 item(s)".into(),
            },
        ];
        assert_eq!(
            replay(&events)[1..],
            [
                TranscriptEntry::Note {
                    text: "team plan: 2 item(s)".into(),
                },
                TranscriptEntry::Note {
                    text: "round 1 started: 2 item(s)".into(),
                },
            ]
        );
        assert_eq!(
            handoff_note(Some("laptop"), "r"),
            "session handed off to another host to laptop (r)"
        );
    }

    #[test]
    fn replay_turns_a_user_shell_event_into_a_shell_entry() {
        let events = vec![
            SessionEvent::UserShell {
                command: "ls".into(),
                output: "a.rs\nb.rs\n".into(),
                is_error: false,
                private: false,
            },
            SessionEvent::UserShell {
                command: "cat .env".into(),
                output: "SECRET=1".into(),
                is_error: true,
                private: true,
            },
        ];
        let entries = replay(&events);
        assert_eq!(
            entries,
            vec![
                TranscriptEntry::Shell {
                    command: "ls".into(),
                    lines: vec!["a.rs".into(), "b.rs".into()],
                    is_error: false,
                    private: false,
                },
                TranscriptEntry::Shell {
                    command: "cat .env".into(),
                    lines: vec!["SECRET=1".into()],
                    is_error: true,
                    private: true,
                },
            ]
        );
    }

    #[test]
    fn old_task_encoded_shell_text_still_replays_as_a_task() {
        let text = "Shell command run by the user with `!` (not a model tool call):\n$ ls\nx";
        let entries = replay(&[SessionEvent::Task { text: text.into() }]);
        assert!(matches!(&entries[..], [TranscriptEntry::Task { .. }]));
    }
}
