//! The built-in demo script for the TUI host: snapshots built from the client
//! core's own types (`entry_lines`, `DECISION_LABELS`), played over time.

use modron_mobile_core::{
    entry_lines, ConnectionState, DecisionOption, PendingApprovalView, RenderedRow,
    ViewModelSnapshot, DECISION_LABELS,
};
use modron_view::TranscriptEntry;

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
    ViewModelSnapshot {
        connection: ConnectionState::Connected,
        connection_label: "connected".into(),
        running,
        running_label: if running { "running" } else { "idle" }.into(),
        status_line: format!("claude-opus-5 \u{b7} {status}"),
        streaming_text: streaming.into(),
        pending_approval: pending.then_some(approval),
        provider: "anthropic".into(),
        history,
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
