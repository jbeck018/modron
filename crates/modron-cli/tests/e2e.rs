//! V2 — the offline end-to-end acceptance test (Definition of Usable #3–4).
//!
//! Two layers, both fully offline:
//!
//! 1. [`mock_drives_real_tools_and_store_end_to_end`] — a scripted
//!    `MockProvider` drives the REAL engine loop with the REAL built-in
//!    tools (`bash`, `read`) on a temp dir and the REAL
//!    `JsonlSessionStore`: bash echoes a marker into a file, read reads it
//!    back, the final answer quotes it. Asserts the marker flows through the
//!    transcript, the session `.jsonl` file exists with Task/Message/
//!    ToolResult/Usage events, `RunSummary.usage` is the sum of the script
//!    usages, and `run_task_resuming` reconstructs the transcript.
//!
//! 2. [`cli_binary_mock_run_and_resume`] — runs the actual `modron` binary
//!    (`--provider mock`) as a subprocess: answer + usage table on stdout,
//!    exit code 0, session file on disk, then `--resume` continues it.

use modron_core::testing::{MockProvider, StaticSelector};
use modron_core::{
    Engine, EngineConfig, ModelProvider, Role, SessionEvent, SessionStore, StopReason, ToolCall,
    TurnResponse, Usage,
};
use modron_store::JsonlSessionStore;
use modron_tools::default_registry;
use serde_json::json;
use std::collections::HashMap;
use std::process::Command;
use std::sync::Arc;

const MARKER: &str = "modron-e2e-marker";

fn tool_call_response(
    id: &str,
    name: &str,
    arguments: serde_json::Value,
    usage: Usage,
) -> TurnResponse {
    TurnResponse {
        text: String::new(),
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: name.into(),
            arguments,
        }],
        usage,
        stop_reason: StopReason::ToolUse,
    }
}

fn text_response(text: String, usage: Usage) -> TurnResponse {
    TurnResponse {
        text,
        tool_calls: Vec::new(),
        usage,
        stop_reason: StopReason::EndTurn,
    }
}

fn mock_engine(
    provider: Arc<dyn ModelProvider>,
    store: Arc<dyn SessionStore>,
    working_dir: &std::path::Path,
) -> Engine {
    let mut providers = HashMap::new();
    providers.insert("mock".to_string(), provider);
    Engine {
        selector: Arc::new(StaticSelector::new("mock-model", "mock")),
        providers,
        tools: Arc::new(default_registry()),
        store,
        config: EngineConfig {
            working_dir: working_dir.to_path_buf(),
            // These tests exercise the tool pipeline, not the approval gate
            // (A1 has its own tests): mutating tools must actually run.
            approvals: modron_core::ApprovalPolicy {
                mode: modron_core::ApprovalMode::ApproveAll,
                non_interactive: true,
                allow: Vec::new(),
                deny: Vec::new(),
            },
            ..EngineConfig::default()
        },
    }
}

#[tokio::test]
async fn mock_drives_real_tools_and_store_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = dir.path().join("sessions");

    // Script: bash writes+echoes the marker, read reads it back, final
    // answer quotes it. Scripted usage must sum exactly into RunSummary.
    let u1 = Usage {
        input_tokens: 100,
        output_tokens: 20,
        cached_input_tokens: 0,
    };
    let u2 = Usage {
        input_tokens: 150,
        output_tokens: 25,
        cached_input_tokens: 10,
    };
    let u3 = Usage {
        input_tokens: 200,
        output_tokens: 30,
        cached_input_tokens: 0,
    };
    let provider = Arc::new(
        MockProvider::new()
            .then_response(tool_call_response(
                "call-1",
                "bash",
                json!({"command": format!("echo {MARKER} | tee marker.txt")}),
                u1,
            ))
            .then_response(tool_call_response(
                "call-2",
                "read",
                json!({"path": "marker.txt"}),
                u2,
            ))
            .then_response(text_response(
                format!("The marker file contains: {MARKER}"),
                u3,
            )),
    );

    let store: Arc<dyn SessionStore> = Arc::new(JsonlSessionStore::new(&sessions_dir));
    let engine = mock_engine(provider.clone(), Arc::clone(&store), dir.path());

    let summary = engine
        .run_task("write and read back the marker")
        .await
        .unwrap();

    // RunSummary accounting: usage is exactly the sum of script usages.
    assert_eq!(
        summary.answer,
        format!("The marker file contains: {MARKER}")
    );
    assert_eq!(summary.turns, 3);
    assert_eq!(summary.tool_calls, 2);
    assert_eq!(
        summary.usage.input_tokens,
        u1.input_tokens + u2.input_tokens + u3.input_tokens
    );
    assert_eq!(
        summary.usage.output_tokens,
        u1.output_tokens + u2.output_tokens + u3.output_tokens
    );
    assert_eq!(
        summary.usage.cached_input_tokens,
        u1.cached_input_tokens + u2.cached_input_tokens + u3.cached_input_tokens
    );
    assert_eq!(summary.models_used, vec!["mock-model".to_string()]);

    // The REAL bash tool ran: the marker file exists in the working dir.
    let marker_on_disk = std::fs::read_to_string(dir.path().join("marker.txt")).unwrap();
    assert_eq!(marker_on_disk.trim(), MARKER);

    // The marker flowed through the transcript via REAL tool results: turn 2
    // saw the bash output, turn 3 saw the read output.
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    let bash_result = requests[1]
        .messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Tool)
        .unwrap();
    assert!(
        bash_result.content.contains(MARKER),
        "bash result: {}",
        bash_result.content
    );
    assert!(bash_result.content.contains("exit code: 0"));
    let read_result = requests[2]
        .messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Tool)
        .unwrap();
    assert!(
        read_result.content.contains(MARKER),
        "read result: {}",
        read_result.content
    );

    // The session .jsonl exists on disk with the expected event types.
    let jsonl_path = sessions_dir.join(format!("{}.jsonl", summary.session_id));
    assert!(
        jsonl_path.exists(),
        "session transcript must be on disk at {}",
        jsonl_path.display()
    );
    assert!(jsonl_path.to_string_lossy().ends_with(".jsonl"));
    let events = store.load(summary.session_id).await.unwrap();
    assert!(
        matches!(events.first(), Some(SessionEvent::Task { text, .. }) if text.contains("marker"))
    );
    assert!(events
        .iter()
        .any(|e| matches!(e, SessionEvent::Message { message, ..
        } if message.role == Role::Assistant && !message.tool_calls.is_empty())));
    assert!(events.iter().any(|e| matches!(e, SessionEvent::ToolResult { call_id, .. } if call_id == "call-1" || call_id == "call-2")));
    assert!(events
        .iter()
        .any(|e| matches!(e, SessionEvent::Usage { .. })));
    let usage_events: Vec<(String, Usage)> = events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::Usage { model, usage } => Some((model.clone(), *usage)),
            _ => None,
        })
        .collect();
    assert_eq!(usage_events.len(), 3, "one Usage event per scripted turn");
    assert_eq!(
        usage_events.iter().map(|(_, u)| u.total()).sum::<u64>(),
        summary.usage.total(),
        "persisted usage events sum to RunSummary.usage"
    );

    // RESUME: a fresh engine + fresh mock continues the SAME session id, and
    // the reconstructed transcript (from the store) reaches the provider.
    let provider2 = Arc::new(MockProvider::new().then_text(format!("resumed: {MARKER}")));
    let engine2 = mock_engine(provider2.clone(), Arc::clone(&store), dir.path());
    let summary2 = engine2
        .run_task_resuming(summary.session_id, "what did the marker say?")
        .await
        .unwrap();

    assert_eq!(
        summary2.session_id, summary.session_id,
        "resume continues the SAME session"
    );
    assert_eq!(summary2.answer, format!("resumed: {MARKER}"));

    let resumed_requests = provider2.requests();
    assert_eq!(resumed_requests.len(), 1);
    let messages = &resumed_requests[0].messages;
    // task + assistant(tool_call) + tool + assistant(tool_call) + tool +
    // assistant(final) + resumed task = 7 messages.
    assert_eq!(
        messages.len(),
        7,
        "reconstructed transcript: {:?}",
        messages.iter().map(|m| m.role).collect::<Vec<_>>()
    );
    assert!(messages
        .iter()
        .any(|m| m.role == Role::Tool && m.content.contains(MARKER)));
    assert_eq!(messages.last().unwrap().role, Role::User);
    assert_eq!(messages.last().unwrap().content, "what did the marker say?");

    // The resumed run appended to the same JSONL log.
    let events_after = store.load(summary.session_id).await.unwrap();
    assert!(
        events_after.len() > events.len(),
        "resume appends to the same session log"
    );
    assert!(
        matches!(events_after.last(), Some(SessionEvent::Message { message, ..
        }) if message.content == format!("resumed: {MARKER}"))
    );
}

/// Definition of Usable #4 as a test: the real binary, `--provider mock`,
/// keyless. Asserts exit code 0, answer + usage table + session line on
/// stdout, the `.jsonl` on disk, and that `--resume` continues the session.
#[test]
fn cli_binary_mock_run_and_resume() {
    let bin = env!("CARGO_BIN_EXE_modron");
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = dir.path().join("sessions");

    let output = Command::new(bin)
        .arg("run the modron demo")
        .args(["--provider", "mock"])
        .arg("--sessions-dir")
        .arg(&sessions_dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "exit {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Mock run complete — the loop, tools, store, and budget all executed."),
        "final answer on stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("usage per model"),
        "usage table on stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("ESTIMATED"),
        "cost must be marked estimated:\n{stdout}"
    );
    assert!(
        stdout.contains("1152"),
        "scripted usage sum (512+640) on stdout:\n{stdout}"
    );

    // Session id line -> transcript file exists.
    let session_line = stdout
        .lines()
        .find(|l| l.starts_with("session: "))
        .expect("session line");
    let session_id = session_line
        .trim_start_matches("session: ")
        .split_whitespace()
        .next()
        .unwrap();
    let jsonl = sessions_dir.join(format!("{session_id}.jsonl"));
    assert!(jsonl.exists(), "transcript at {}", jsonl.display());

    // Resume the same session through the binary.
    let resumed = Command::new(bin)
        .arg("continue the demo")
        .args(["--provider", "mock"])
        .arg("--sessions-dir")
        .arg(&sessions_dir)
        .args(["--resume=", session_id])
        .output()
        .unwrap();
    assert!(
        resumed.status.success(),
        "resume exit {:?}\nstderr: {}",
        resumed.status.code(),
        String::from_utf8_lossy(&resumed.stderr)
    );
    let resumed_stdout = String::from_utf8_lossy(&resumed.stdout);
    assert!(
        resumed_stdout.contains("Mock run complete"),
        "resumed answer:\n{resumed_stdout}"
    );
    assert!(
        resumed_stdout.contains(session_id),
        "resume reports the same session id:\n{resumed_stdout}"
    );
    // Per-run usage scoping: the resumed table shows only THIS run's usage.
    assert!(
        resumed_stdout.contains("1152"),
        "resumed run reports only its own usage:\n{resumed_stdout}"
    );
    assert!(
        !resumed_stdout.contains("2304"),
        "resumed table must not double-count past usage:\n{resumed_stdout}"
    );

    // Transcript grew in place: still one file, more lines.
    let lines = std::fs::read_to_string(&jsonl).unwrap().lines().count();
    assert!(
        lines >= 16,
        "both runs' events in one log, got {lines} lines"
    );
}
