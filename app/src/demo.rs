//! The built-in demo script for the TUI host: snapshots built from the client
//! core's own types (`entry_lines`, `DECISION_LABELS`), played over time.

use modron_mobile_core::{
    connection_label, entry_lines, session_row, ConnectionState, DecisionOption,
    PendingApprovalView, RenderedRow, SessionMetaWire, ViewModelSnapshot, DECISION_LABELS,
};
use modron_view::TranscriptEntry;
use uuid::Uuid;

use crate::chat::Host;

/// The demo's sessions: the one running, two idle, so both markers show.
fn demo_sessions() -> Vec<SessionMetaWire> {
    let meta = |n: u128, title: &str, count: u32, running: bool| SessionMetaWire {
        id: Uuid::from_u128(n),
        title: title.into(),
        created_at: "2026-11-15T09:00:00Z".into(),
        updated_at: "2026-11-15T09:30:00Z".into(),
        message_count: count,
        task_branch: None,
        task_state: running.then(|| "running".to_string()),
        working_dir: None,
    };
    vec![
        meta(0xd1, "fix the router crate", 6, true),
        meta(0xd2, "write the release notes", 14, false),
        meta(0xd3, "triage flaky tests", 3, false),
    ]
}

fn row(entry: TranscriptEntry) -> RenderedRow {
    RenderedRow { lines: entry_lines(&entry), entry }
}

/// A snapshot shaped as the core hands it over; `status` is the status tail.
fn snap(
    status: &str,
    running: bool,
    streaming: &str,
    history: Vec<RenderedRow>,
    pending: bool,
) -> ViewModelSnapshot {
    let approval = PendingApprovalView {
        request_id: "demo-1".into(),
        tool: "edit".into(),
        args_summary: "crates/modron-router/src/lib.rs".into(),
        arguments: serde_json::json!({}),
        arguments_text: String::new(),
        options: DECISION_LABELS
            .iter()
            .map(|(decision, label)| DecisionOption { decision: *decision, label: (*label).into() })
            .collect(),
    };
    let sessions = demo_sessions();
    let session_rows =
        sessions.iter().map(|meta| session_row(meta, meta.task_state.is_some())).collect();
    ViewModelSnapshot {
        connection: ConnectionState::Connected,
        connection_label: connection_label(&ConnectionState::Connected).into(),
        running,
        running_label: if running { "running" } else { "idle" }.into(),
        status_line: format!("claude-opus-5 \u{b7} {status}"),
        streaming_text: streaming.into(),
        pending_approval: pending.then_some(approval),
        provider: "anthropic".into(),
        model: "claude-opus-5".into(),
        approval_mode_label: "ask".into(),
        phase_label: "build".into(),
        sandbox: "off".into(),
        history,
        sessions,
        session_rows,
        ..Default::default()
    }
}

/// (ms from start, snapshot) pairs; the last frame holds.
pub fn script() -> Vec<(u64, ViewModelSnapshot)> {
    let task = row(TranscriptEntry::Task { text: "fix the router crate".into() });
    let reply = row(TranscriptEntry::Assistant { text: "Looking at the router crate.".into() });
    let model =
        row(TranscriptEntry::Model { model: "claude-opus-5".into(), reason: "default".into() });
    let reading = "Reading the router and its tests; ";
    vec![
        (0, snap("0 tok \u{b7} ask", false, "", vec![task.clone()], false)),
        (800, snap("1.2k tok \u{b7} ask", true, "Reading the router ", vec![task.clone()], false)),
        (2_500, snap("2.4k tok \u{b7} ask", true, reading, vec![task.clone()], false)),
        (4_500, snap("2.4k tok \u{b7} ask", true, reading, vec![task.clone()], true)),
        (7_000, snap("6.1k tok \u{b7} ask", false, "", vec![task, reply, model], false)),
    ]
}

/// Plays [`script`] into a host: each frame is pushed once its time has come.
/// Both hosts drive it from their own clock.
pub struct Player {
    frames: std::iter::Peekable<std::vec::IntoIter<(u64, ViewModelSnapshot)>>,
}

impl Default for Player {
    fn default() -> Self {
        Self { frames: script().into_iter().peekable() }
    }
}

impl Player {
    /// Pushes every frame due at `elapsed_ms` (since the demo started).
    /// Returns true when a frame was pushed.
    pub fn pump(&mut self, host: &mut Host, elapsed_ms: u64) -> bool {
        // No driver in the demo: taps and keys still apply their navigation,
        // and the core calls they imply go nowhere.
        while host.try_action().is_some() {}
        let mut pushed = false;
        while let Some((at, _)) = self.frames.peek() {
            if *at > elapsed_ms {
                break;
            }
            let (_, snapshot) = self.frames.next().expect("peeked");
            host.push(&snapshot);
            pushed = true;
        }
        pushed
    }
}
