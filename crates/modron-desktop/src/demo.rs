//! [`demo`] -- a built-in mock modron server for standalone runs.
//!
//! `modron-desktop --demo` starts this listener inside the app's runtime
//! and connects the real [`crate::wire`] client to it, so the full
//! protocol path (attach, snapshot, streaming deltas, tool activity, a
//! subagent, one approval round-trip, turn done) is exercised without the
//! Phase-2 server present. Everything it speaks is the real wire
//! vocabulary from modron-protocol.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use futures::{SinkExt, StreamExt};
use modron_core::{Message, SessionEvent, ToolCall, ToolOutcome, Usage};
use modron_protocol::{
    Decision, Frame, LiveState, Payload, SessionMetaWire, WireDelta, WireRunSummary,
    WireToolActivity,
};
use tokio::sync::{mpsc, watch};

use crate::wire::{accept_ws, frame_message, parse_frame};

/// Session A: a finished history, idle (attach replays it).
pub const SESSION_A: uuid::Uuid = uuid::Uuid::from_u128(0xA001);
/// Session B: a smaller history; attaching plays a scripted live run.
pub const SESSION_B: uuid::Uuid = uuid::Uuid::from_u128(0xB002);

const SERVER_VERSION: &str = "modron-demo/0.1.2";
const MODEL: &str = "claude-haiku-5-5";
const PROVIDER: &str = "anthropic";

/// Start the demo listener on a fresh loopback port and return its
/// `ws://` URL.
pub async fn spawn() -> Result<String> {
    let listener = crate::wire::bind_loopback().await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        let mut listener = listener;
        while let Ok(ws) = accept_ws(&mut listener).await {
            tokio::spawn(async move {
                if let Err(e) = serve_connection(ws).await {
                    eprintln!("demo server connection ended: {e:#}");
                }
            });
        }
    });
    Ok(format!("ws://127.0.0.1:{port}"))
}

/// Per-connection server state.
struct Demo {
    sessions: Mutex<Vec<SessionMetaWire>>,
    seqs: Mutex<HashMap<uuid::Uuid, u64>>,
    approval_tx: Mutex<Option<mpsc::Sender<Decision>>>,
    run_abort: Mutex<Option<tokio::task::AbortHandle>>,
    attached: watch::Sender<Option<uuid::Uuid>>,
}

impl Demo {
    fn new() -> Arc<Self> {
        let sessions = vec![
            SessionMetaWire {
                id: SESSION_A,
                title: "Refactor the config loader".into(),
                created_at: "2026-11-09T09:00:00Z".into(),
                updated_at: "2026-11-09T09:14:00Z".into(),
                message_count: 4,
                task_branch: None,
                task_state: None,
                working_dir: Some("/Users/jacob/dev/modron".into()),
            },
            SessionMetaWire {
                id: SESSION_B,
                title: "Fix flaky websocket test".into(),
                created_at: "2026-11-09T10:00:00Z".into(),
                updated_at: "2026-11-09T10:30:00Z".into(),
                message_count: 2,
                task_branch: None,
                task_state: None,
                working_dir: Some("/Users/jacob/dev/bridge".into()),
            },
        ];
        let (attached, _) = watch::channel(None);
        Arc::new(Self {
            sessions: Mutex::new(sessions),
            seqs: Mutex::new(HashMap::new()),
            approval_tx: Mutex::new(None),
            run_abort: Mutex::new(None),
            attached,
        })
    }

    fn next_seq(&self, session: uuid::Uuid) -> u64 {
        let mut seqs = self.seqs.lock().expect("seqs lock");
        let next = seqs.entry(session).or_insert(0);
        *next += 1;
        *next
    }

    fn current_seq(&self, session: uuid::Uuid) -> u64 {
        *self
            .seqs
            .lock()
            .expect("seqs lock")
            .get(&session)
            .unwrap_or(&0)
    }

    async fn send_sessions(&self, tx: &mpsc::Sender<Frame>) {
        let payload = Payload::Sessions {
            sessions: self.sessions.lock().expect("sessions lock").clone(),
        };
        let _ = tx.send(Frame::event(None, 0, payload)).await;
    }

    fn create_session(&self, title: &str) -> uuid::Uuid {
        let id = uuid::Uuid::new_v4();
        self.sessions
            .lock()
            .expect("sessions lock")
            .push(SessionMetaWire {
                id,
                title: title.chars().take(40).collect(),
                created_at: "2026-11-09T11:00:00Z".into(),
                updated_at: "2026-11-09T11:00:00Z".into(),
                message_count: 0,
                task_branch: None,
                task_state: None,
                working_dir: Some("/Users/jacob/dev/modron".into()),
            });
        id
    }
}

async fn serve_connection(ws: crate::wire::WsStream) -> Result<()> {
    let (mut sink, mut stream) = ws.split();
    let (tx, mut rx) = mpsc::channel::<Frame>(64);
    let writer = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            if sink.send(frame_message(&frame)).await.is_err() {
                break;
            }
        }
    });

    let demo = Demo::new();
    while let Some(msg) = stream.next().await {
        let msg = msg?;
        let Some(frame) = parse_frame(&msg) else {
            continue;
        };
        handle_frame(&demo, frame, &tx).await;
    }
    writer.abort();
    Ok(())
}

async fn handle_frame(demo: &Arc<Demo>, frame: Frame, tx: &mpsc::Sender<Frame>) {
    match frame.payload {
        Payload::Hello { protocol, .. } => {
            let _ = tx
                .send(Frame::event(
                    None,
                    0,
                    Payload::Welcome {
                        server_version: SERVER_VERSION.into(),
                        protocol,
                    },
                ))
                .await;
            demo.send_sessions(tx).await;
        }
        Payload::ListSessions {} => demo.send_sessions(tx).await,
        Payload::AuthProvidersReq {} | Payload::AuthProviders { .. } => {
            // The demo has no providers; answer so the settings page renders.
            let _ = tx
                .send(Frame::event(
                    None,
                    0,
                    Payload::AuthProviders { rows: Vec::new() },
                ))
                .await;
        }
        Payload::Ping {} => {
            let mut pong = Frame::event(None, 0, Payload::Pong {});
            pong.id = frame.id;
            let _ = tx.send(pong).await;
        }
        Payload::Attach { session_id, .. } => {
            let events = fixture_events(session_id);
            demo.seqs
                .lock()
                .expect("seqs lock")
                .entry(session_id)
                .or_insert(events.len() as u64);
            let running = demo.run_abort.lock().expect("run lock").is_some()
                && *demo.attached.subscribe().borrow() == Some(session_id);
            let live = LiveState {
                model: MODEL.into(),
                provider: PROVIDER.into(),
                cumulative_usage: modron_view::usage_totals(&events),
                running,
                ..LiveState::default()
            };
            let payload = Payload::Snapshot {
                session_id,
                last_seq: demo.current_seq(session_id),
                events,
                live,
            };
            let _ = tx
                .send(Frame::event(
                    Some(session_id),
                    demo.current_seq(session_id),
                    payload,
                ))
                .await;
            let _ = demo.attached.send(Some(session_id));
            start_scripted_run(demo, tx.clone(), session_id, None);
        }
        Payload::Detach {} => {
            let _ = demo.attached.send(None);
        }
        Payload::Submit {
            session_id, task, ..
        } => {
            let id = match session_id {
                Some(id) => id,
                None => {
                    let id = demo.create_session(&task);
                    demo.send_sessions(tx).await;
                    id
                }
            };
            start_scripted_run(demo, tx.clone(), id, Some(task));
        }
        Payload::NewSession { .. } => {
            demo.create_session("untitled");
            demo.send_sessions(tx).await;
        }
        Payload::Interrupt { session_id } => {
            if let Some(handle) = demo.run_abort.lock().expect("run lock").take() {
                handle.abort();
            }
            *demo.approval_tx.lock().expect("approval lock") = None;
            let _ = tx
                .send(Frame::event(
                    Some(session_id),
                    demo.next_seq(session_id),
                    Payload::TurnAborted {},
                ))
                .await;
        }
        Payload::Approve { decision, .. } => {
            // First responder wins: forward to the waiting run. The guard
            // is dropped before the await (std Mutex is not held across
            // yield points).
            let sender = demo.approval_tx.lock().expect("approval lock").take();
            if let Some(sender) = sender {
                let _ = sender.send(decision).await;
            }
        }
        Payload::SetConfig { patch } => {
            // Applied for future turns; echo the applied patch. The reply
            // carries the request id (correlation contract).
            let mut reply = Frame::event(None, 0, Payload::ConfigChanged { patch });
            reply.id = frame.id;
            let _ = tx.send(reply).await;
        }
        Payload::Fork { session_id } => {
            let copy = demo.create_session("fork");
            let _ = tx
                .send(Frame::event(
                    None,
                    0,
                    Payload::Note {
                        text: format!("forked session {} as {copy}", &session_id.to_string()[..8]),
                    },
                ))
                .await;
            demo.send_sessions(tx).await;
        }
        // Client-only or unhandled server payloads.
        _ => {}
    }
}

/// Kick off the scripted live run for `session` (used by both `attach`
/// and `submit`; with `task` set, a Task event lands first).
fn start_scripted_run(
    demo: &Arc<Demo>,
    tx: mpsc::Sender<Frame>,
    session: uuid::Uuid,
    task: Option<String>,
) {
    // One run at a time in the demo: replace any previous one.
    if let Some(handle) = demo.run_abort.lock().expect("run lock").take() {
        handle.abort();
    }
    let (approval_tx, approval_rx) = mpsc::channel::<Decision>(1);
    *demo.approval_tx.lock().expect("approval lock") = Some(approval_tx);
    let run_demo = Arc::clone(demo);
    let handle = tokio::spawn(async move {
        scripted_run(run_demo, tx, session, task, approval_rx).await;
    });
    *demo.run_abort.lock().expect("run lock") = Some(handle.abort_handle());
}

/// Pause between scripted steps so the streaming is visible in the UI.
async fn beat() {
    tokio::time::sleep(std::time::Duration::from_millis(220)).await;
}

/// Emit one session event frame with the next seq.
async fn emit(demo: &Arc<Demo>, tx: &mpsc::Sender<Frame>, session: uuid::Uuid, payload: Payload) {
    let frame = Frame::event(Some(session), demo.next_seq(session), payload);
    let _ = tx.send(frame).await;
}

async fn scripted_run(
    demo: Arc<Demo>,
    tx: mpsc::Sender<Frame>,
    session: uuid::Uuid,
    task: Option<String>,
    mut approval_rx: mpsc::Receiver<Decision>,
) {
    // A fresh submit persists the task echo first.
    if let Some(task) = task {
        emit(
            &demo,
            &tx,
            session,
            Payload::Event {
                event: SessionEvent::Task { text: task },
            },
        )
        .await;
    }

    for chunk in ["Let me look ", "at the failing ", "websocket test first…"] {
        beat().await;
        emit(
            &demo,
            &tx,
            session,
            Payload::Delta {
                delta: WireDelta::Text { text: chunk.into() },
            },
        )
        .await;
    }

    beat().await;
    // Tools persist the way the engine does: one assistant message
    // carrying the turn's tool_calls, then the ToolResult events, while
    // the live Tool frames stream the activity. A replay rebuilds the
    // same grouped view ("one summary line per turn").
    let call_read = format!("call-{session:032x}-run-0");
    let call_bash = format!("call-{session:032x}-run-1");
    let mut calls_message = Message::assistant("");
    calls_message.tool_calls = vec![
        ToolCall {
            id: call_read.clone(),
            name: "read".into(),
            arguments: serde_json::json!({"path": "tests/ws.rs"}),
        },
        ToolCall {
            id: call_bash.clone(),
            name: "bash".into(),
            arguments: serde_json::json!({"cmd": "cargo test"}),
        },
    ];
    emit(
        &demo,
        &tx,
        session,
        Payload::Event {
            event: SessionEvent::Message {
                parent_id: None,
                message: calls_message,
            },
        },
    )
    .await;
    emit(
        &demo,
        &tx,
        session,
        Payload::Tool {
            activity: WireToolActivity::Start {
                name: "read".into(),
            },
        },
    )
    .await;
    beat().await;
    emit(
        &demo,
        &tx,
        session,
        Payload::Tool {
            activity: WireToolActivity::Done {
                name: "read".into(),
                is_error: false,
                bytes: 2150,
            },
        },
    )
    .await;
    emit(
        &demo,
        &tx,
        session,
        Payload::Event {
            event: SessionEvent::ToolResult {
                call_id: call_read,
                outcome: ToolOutcome::ok("fn handshake() { ... }"),
            },
        },
    )
    .await;
    beat().await;
    emit(
        &demo,
        &tx,
        session,
        Payload::Tool {
            activity: WireToolActivity::Start {
                name: "bash".into(),
            },
        },
    )
    .await;
    beat().await;
    emit(
        &demo,
        &tx,
        session,
        Payload::Tool {
            activity: WireToolActivity::Done {
                name: "bash".into(),
                is_error: false,
                bytes: 512,
            },
        },
    )
    .await;
    emit(
        &demo,
        &tx,
        session,
        Payload::Event {
            event: SessionEvent::ToolResult {
                call_id: call_bash,
                outcome: ToolOutcome::ok("cargo test ... ok"),
            },
        },
    )
    .await;

    // A subagent sweeps the codebase while the main line waits.
    beat().await;
    emit(
        &demo,
        &tx,
        session,
        Payload::Tool {
            activity: WireToolActivity::SubagentStarted {
                id: "sub-1".into(),
                label: "explore".into(),
            },
        },
    )
    .await;
    for turns in 1..=2u32 {
        beat().await;
        emit(
            &demo,
            &tx,
            session,
            Payload::Tool {
                activity: WireToolActivity::SubagentProgress {
                    id: "sub-1".into(),
                    label: "explore".into(),
                    turns,
                    tokens: 250 * turns as u64,
                    last_tool: Some("grep".into()),
                },
            },
        )
        .await;
    }
    beat().await;
    emit(
        &demo,
        &tx,
        session,
        Payload::Tool {
            activity: WireToolActivity::SubagentFinished {
                id: "sub-1".into(),
                label: "explore".into(),
                tokens: 500,
                turns: 2,
                is_error: false,
            },
        },
    )
    .await;

    // One mutating tool asks for a decision; the demo waits for the
    // client (first responder wins), defaulting to deny after a minute.
    beat().await;
    emit(&demo, &tx, session, Payload::ApprovalRequested {
        request_id: "demo-approval-1".into(),
        call: ToolCall {
            id: "call-demo-1".into(),
            name: "edit".into(),
            arguments: serde_json::json!({"path": "src/net.rs", "change": "retry once on timeout"}),
        },
    })
    .await;
    let decision = tokio::time::timeout(std::time::Duration::from_secs(60), approval_rx.recv())
        .await
        .ok()
        .and_then(|d| d)
        .unwrap_or(Decision::Deny);
    beat().await;
    emit(
        &demo,
        &tx,
        session,
        Payload::ApprovalResolved {
            request_id: "demo-approval-1".into(),
            decision,
        },
    )
    .await;

    for chunk in ["Applied ", "the fix ", "to src/net.rs."] {
        beat().await;
        emit(
            &demo,
            &tx,
            session,
            Payload::Delta {
                delta: WireDelta::Text { text: chunk.into() },
            },
        )
        .await;
    }

    // Commit the run: assistant message, usage, turn summary.
    beat().await;
    emit(&demo, &tx, session, Payload::Event {
        event: SessionEvent::Message {
            parent_id: None,
            message: Message::assistant("Applied the fix to src/net.rs. The retry-once patch makes the websocket handshake test deterministic; full suite passes."),
        },
    })
    .await;
    emit(
        &demo,
        &tx,
        session,
        Payload::Event {
            event: SessionEvent::Usage {
                model: MODEL.into(),
                usage: Usage {
                    input_tokens: 640,
                    output_tokens: 88,
                    cached_input_tokens: 310,
                },
            },
        },
    )
    .await;
    beat().await;
    emit(
        &demo,
        &tx,
        session,
        Payload::TurnDone {
            summary: WireRunSummary {
                session_id: session,
                answer: "Applied the fix to src/net.rs.".into(),
                turns: 1,
                tool_calls: 2,
                usage: Usage {
                    input_tokens: 640,
                    output_tokens: 88,
                    cached_input_tokens: 310,
                },
                models_used: vec![MODEL.into()],
            },
        },
    )
    .await;
    *demo.approval_tx.lock().expect("approval lock") = None;
    if let Some(handle) = demo.run_abort.lock().expect("run lock").take() {
        handle.abort();
    }
}

/// The persisted history per fixture session (session B's log is the
/// shorter one).
fn fixture_events(session: uuid::Uuid) -> Vec<SessionEvent> {
    let mut read_call = Message::assistant("Reproducing the flake locally.");
    read_call.tool_calls = vec![ToolCall {
        id: format!("call-{session:032x}-1"),
        name: "read".into(),
        arguments: serde_json::json!({"path": "tests/ws.rs"}),
    }];
    let usage = Usage {
        input_tokens: 812,
        output_tokens: 96,
        cached_input_tokens: 402,
    };
    let mut events = vec![
        SessionEvent::Task {
            text: if session == SESSION_B {
                "Fix flaky websocket test".into()
            } else {
                "Refactor the config loader".into()
            },
        },
        SessionEvent::Message {
            parent_id: None,
            message: read_call,
        },
        SessionEvent::ToolResult {
            call_id: format!("call-{session:032x}-1"),
            outcome: ToolOutcome::ok("fn handshake() { /* retry logic missing */ }"),
        },
        SessionEvent::Message {
            parent_id: None,
            message: Message::assistant(if session == SESSION_B {
                "The handshake has no retry; a slow accept makes the test flake."
            } else {
                "Split config.rs into load and validate; two call sites updated."
            }),
        },
        SessionEvent::Usage {
            model: MODEL.into(),
            usage,
        },
    ];
    if session == SESSION_A {
        events.push(SessionEvent::ModelSelected {
            model: MODEL.into(),
            reason: "default".into(),
            action: None,
            context_tokens: None,
        });
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_state::{AppState, Row};
    use crate::wire::WireClient;
    use modron_view::TranscriptEntry;
    use tokio::time::{timeout, Duration};

    /// Drive the whole demo protocol through the real client into a real
    /// AppState: handshake, list, attach, scripted live run with the
    /// approval round-trip, commit at TurnDone.
    #[tokio::test]
    async fn demo_server_speaks_the_protocol_end_to_end() {
        let url = spawn().await.expect("demo server");
        let (mut client, server_version) = WireClient::connect(&url).await.expect("connect");
        assert!(server_version.starts_with("modron-demo/"));

        client.send(Payload::ListSessions {}).await.expect("list");
        client
            .send(Payload::Attach {
                session_id: SESSION_B,
                last_seq: 0,
            })
            .await
            .expect("attach");

        let mut state = AppState::new();
        let mut approved = false;
        let deadline = Duration::from_secs(30);
        loop {
            let frame = timeout(deadline, client.recv())
                .await
                .expect("frame within deadline")
                .expect("open connection");
            let is_done = matches!(frame.payload, Payload::TurnDone { .. });
            if let Payload::ApprovalRequested { request_id, .. } = &frame.payload {
                assert!(!approved, "one approval per scripted run");
                approved = true;
                client
                    .send(Payload::Approve {
                        request_id: request_id.clone(),
                        decision: Decision::AllowOnce,
                    })
                    .await
                    .expect("approve");
            }
            state.apply_frame(frame);
            if is_done {
                break;
            }
        }

        // The projections landed through the whole stack.
        assert!(approved, "the scripted run asked for an approval");
        assert_eq!(state.model, MODEL);
        assert_eq!(state.provider, PROVIDER);
        assert!(!state.running);
        assert_eq!(state.turn_count, 1);
        assert_eq!(state.tool_count, 2);
        assert!(state.streaming_text.is_empty(), "committed at turn done");
        let rows = state.rows();
        assert!(
            rows.iter()
                .any(|r| matches!(r, Row::Entry(TranscriptEntry::Swarm { .. }))),
            "the swarm block commits: {rows:?}"
        );
        assert!(rows.iter().any(|r| matches!(
            r,
            Row::Entry(TranscriptEntry::ToolSummary { line, .. }) if line.starts_with("⏺ 2 tools")
        )));
    }

    #[tokio::test]
    async fn demo_config_echo_updates_labels() {
        let url = spawn().await.expect("demo server");
        let (mut client, _) = WireClient::connect(&url).await.expect("connect");
        let reply = client
            .request(Payload::SetConfig {
                patch: crate::app_state::model_patch("gpt-5.6-sol"),
            })
            .await
            .expect("config echo");
        match reply.payload {
            Payload::ConfigChanged { ref patch } => {
                assert_eq!(patch.model.as_deref(), Some("gpt-5.6-sol"));
                assert_eq!(patch.provider.as_deref(), Some("chatgpt"));
            }
            other => panic!("expected config_changed, got {other:?}"),
        }

        let mut state = AppState::new();
        state.apply_frame(reply);
        assert_eq!(state.model, "gpt-5.6-sol");
        assert_eq!(state.provider, "chatgpt");
    }

    #[tokio::test]
    async fn demo_interrupt_aborts_the_run() {
        let url = spawn().await.expect("demo server");
        let (mut client, _) = WireClient::connect(&url).await.expect("connect");
        client
            .send(Payload::Submit {
                session_id: None,
                task: "long scripted run".into(),
                attachments: Vec::new(),
            })
            .await
            .expect("submit");
        // Wait for the run to produce at least one frame, then interrupt.
        let mut state = AppState::new();
        loop {
            let frame = timeout(Duration::from_secs(30), client.recv())
                .await
                .expect("frame")
                .expect("open");
            let aborted = matches!(frame.payload, Payload::TurnAborted {});
            let streaming = matches!(frame.payload, Payload::Delta { .. });
            state.apply_frame(frame);
            if aborted {
                assert!(!state.running);
                break;
            }
            if streaming {
                // Newest session first in the sidebar (the fresh submit).
                let newest = state.sessions.first().map(|s| s.id).expect("session");
                client
                    .send(Payload::Interrupt { session_id: newest })
                    .await
                    .expect("interrupt");
            }
        }
    }

    #[tokio::test]
    async fn demo_approves_with_shared_labels() {
        use modron_protocol::{ApprovalModeWire, ConfigPatch, PROTOCOL_VERSION};
        // Sanity: ApprovalModeWire round-trips through ConfigChanged echo.
        let url = spawn().await.expect("demo server");
        let (mut client, _) = WireClient::connect(&url).await.expect("connect");
        let reply = client
            .request(Payload::SetConfig {
                patch: ConfigPatch {
                    approval_mode: Some(ApprovalModeWire::ApproveAll),
                    ..ConfigPatch::default()
                },
            })
            .await
            .expect("echo");
        assert!(matches!(
            reply.payload,
            Payload::ConfigChanged { patch } if patch.approval_mode == Some(ApprovalModeWire::ApproveAll)
        ));
        assert_eq!(PROTOCOL_VERSION, 1);
    }
}
