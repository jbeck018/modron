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

pub use gen_ui::{
    render_ascii, split_generative_ui, ChartKind, GenField, GenFieldKind, GenPoint, GenSegment,
    GenUi,
};

use modron_types::{SessionEvent, ToolActivity, Usage};
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
pub fn human_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// One subagent row of the swarm view (serde-safe: no `Instant`; the TUI
/// keeps its richer local row for elapsed-time rendering).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmRow {
    pub id: String,
    pub label: String,
    pub turns: u32,
    pub tokens: u64,
    pub last_tool: Option<String>,
    /// `None` = still running.
    pub done: Option<bool>,
}

impl SwarmRow {
    /// The display row, e.g. `⏵ [reviewer] done · 3 turns · 1.2k tok ·
    /// last: read`.
    pub fn summary(&self) -> String {
        let mark = match self.done {
            Some(true) => "done",
            Some(false) => "failed",
            None => "running",
        };
        format!(
            "⏵ [{}] {} · {} turns · {} tok · last: {}",
            self.label,
            mark,
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
            ToolActivity::Start { name } => {
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
                }
            }
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

    pub fn is_idle(&self) -> bool {
        self.turn_tools.is_empty() && self.subagents.is_empty()
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
    /// Model-selection note.
    Model { model: String, reason: String },
    /// Usage accounting for one turn.
    Usage { model: String, usage: Usage },
    /// Compaction note.
    Compaction {
        summary: String,
        summarized_events: u32,
    },
    /// A generative-UI widget parsed from a ```modron-ui fenced block in
    /// an assistant answer (table, chart, form). The GUI renders native
    /// widgets; the TUI renders the ASCII projection.
    GenerativeUi { ui: crate::gen_ui::GenUi },
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
pub fn replay(events: &[SessionEvent]) -> Vec<TranscriptEntry> {
    let mut entries: Vec<TranscriptEntry> = Vec::new();
    // call_id -> tool name from the most recent assistant message.
    let mut call_names: std::collections::HashMap<String, String> = Default::default();
    let mut turn_tools: Vec<ToolEvent> = Vec::new();

    let flush_tools = |entries: &mut Vec<TranscriptEntry>, tools: &mut Vec<ToolEvent>| {
        if tools.is_empty() {
            return;
        }
        let line = tool_summary_line(tools);
        entries.push(TranscriptEntry::ToolSummary {
            line,
            events: std::mem::take(tools),
        });
    };

    for event in events {
        match event {
            SessionEvent::Task { text } => {
                flush_tools(&mut entries, &mut turn_tools);
                entries.push(TranscriptEntry::Task { text: text.clone() });
            }
            SessionEvent::Message { message, .. } => match message.role {
                modron_types::Role::User => {
                    flush_tools(&mut entries, &mut turn_tools);
                    entries.push(TranscriptEntry::User {
                        text: message.content.clone(),
                    });
                }
                modron_types::Role::Assistant => {
                    flush_tools(&mut entries, &mut turn_tools);
                    for call in &message.tool_calls {
                        call_names.insert(call.id.clone(), call.name.clone());
                    }
                    if !message.content.is_empty() {
                        // Generative-UI split: ```modron-ui blocks become
                        // native-widget entries; the surrounding prose
                        // stays as assistant text.
                        for segment in gen_ui::split_generative_ui(&message.content) {
                            match segment {
                                gen_ui::GenSegment::Text(text) => {
                                    entries.push(TranscriptEntry::Assistant { text });
                                }
                                gen_ui::GenSegment::Ui(ui) => {
                                    entries.push(TranscriptEntry::GenerativeUi { ui });
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
                let name = call_names
                    .get(call_id)
                    .cloned()
                    .unwrap_or_else(|| "tool".to_string());
                turn_tools.push(ToolEvent {
                    name,
                    bytes: Some(outcome.content.len() as u64),
                    is_error: outcome.is_error,
                    done: true,
                });
            }
            SessionEvent::ModelSelected { model, reason, .. } => {
                flush_tools(&mut entries, &mut turn_tools);
                entries.push(TranscriptEntry::Model {
                    model: model.clone(),
                    reason: reason.clone(),
                });
            }
            SessionEvent::Usage { model, usage } => {
                flush_tools(&mut entries, &mut turn_tools);
                entries.push(TranscriptEntry::Usage {
                    model: model.clone(),
                    usage: *usage,
                });
            }
            SessionEvent::Compaction {
                summary,
                summarized_events,
            } => {
                flush_tools(&mut entries, &mut turn_tools);
                entries.push(TranscriptEntry::Compaction {
                    summary: summary.clone(),
                    summarized_events: *summarized_events,
                });
            }
            SessionEvent::TurnTrace { .. } | SessionEvent::Checkpoint { .. } => {}
        }
    }
    flush_tools(&mut entries, &mut turn_tools);
    entries
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

    fn done(name: &str, bytes: u64, is_error: bool) -> ToolActivity {
        ToolActivity::Done {
            name: name.to_string(),
            is_error,
            bytes: bytes as usize,
        }
    }

    #[test]
    fn groups_start_then_done_in_place() {
        let mut t = ActivityTracker::new();
        t.on_tool(&ToolActivity::Start {
            name: "read".into(),
        });
        t.on_tool(&ToolActivity::Start {
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
                name: "glob".into(),
            });
            t.on_tool(&done("glob", 1, false));
        }
        for _ in 0..8 {
            t.on_tool(&ToolActivity::Start {
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
                TranscriptEntry::Model { .. } => "model",
                TranscriptEntry::Usage { .. } => "usage",
                TranscriptEntry::Compaction { .. } => "compaction",
                TranscriptEntry::GenerativeUi { .. } => "gen_ui",
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
}
