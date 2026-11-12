//! Integration tests: a scripted in-process WebSocket server on
//! 127.0.0.1:0 drives the client through the whole protocol surface —
//! handshake, attach/snapshot folding, live streaming + commit, approval,
//! interrupt, disconnect/reconnect resume.

use futures_util::{SinkExt, StreamExt};
use modron_mobile_core::{ClientError, RemoteClient};
use modron_protocol::{
    ConfigPatch, Decision, Frame, LiveState, Payload, WireDelta, WireToolActivity, PROTOCOL_VERSION,
};
use modron_view::{ToolEvent, TranscriptEntry};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{accept_async, WebSocketStream};
use uuid::Uuid;

type Ws = WebSocketStream<TcpStream>;

async fn bind_server() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").await.expect("bind")
}

async fn url_of(listener: &TcpListener) -> String {
    format!("ws://{}", listener.local_addr().expect("addr"))
}

async fn accept_ws(listener: &TcpListener) -> Ws {
    let (stream, _) = listener.accept().await.expect("accept");
    accept_async(stream).await.expect("ws accept")
}

/// Read the next text frame (skipping ping/pong control frames).
async fn read_frame(ws: &mut Ws) -> Frame {
    loop {
        let msg = ws.next().await.expect("server: socket closed mid-script");
        match msg.expect("server: read error") {
            WsMessage::Text(json) => {
                return serde_json::from_str(&json).expect("server: bad frame json")
            }
            WsMessage::Ping(_) | WsMessage::Pong(_) => continue,
            other => panic!("server: unexpected non-text frame: {other:?}"),
        }
    }
}

async fn send_frame(ws: &mut Ws, frame: &Frame) {
    let json = serde_json::to_string(frame).expect("server: encode");
    ws.send(WsMessage::Text(json.into()))
        .await
        .expect("server: send");
}

/// Server-side handshake: read hello, verify protocol, send welcome.
async fn greet(ws: &mut Ws) {
    let hello = read_frame(ws).await;
    match hello.payload {
        Payload::Hello {
            client,
            protocol,
            token,
        } => {
            assert_eq!(client, "modron-mobile");
            assert_eq!(protocol, PROTOCOL_VERSION);
            assert_eq!(token, None);
        }
        other => panic!("expected hello, got {other:?}"),
    }
    send_frame(
        ws,
        &Frame::event(
            None,
            0,
            Payload::Welcome {
                server_version: "0.1.2".into(),
                protocol: PROTOCOL_VERSION,
            },
        ),
    )
    .await;
}

fn session_id() -> Uuid {
    Uuid::new_v4()
}

fn text_delta(text: &str, seq: u64, session: Uuid) -> Frame {
    Frame::event(
        Some(session),
        seq,
        Payload::Delta {
            delta: WireDelta::Text {
                text: text.to_string(),
            },
        },
    )
}

fn snapshot_frame(
    session: Uuid,
    last_seq: u64,
    events: Vec<modron_types::SessionEvent>,
    live: LiveState,
) -> Frame {
    Frame::event(
        Some(session),
        last_seq,
        Payload::Snapshot {
            session_id: session,
            last_seq,
            events,
            live,
        },
    )
}

fn entry_texts(client: &RemoteClient) -> Vec<String> {
    client
        .state()
        .history
        .iter()
        .map(|row| row.lines.join("\n"))
        .collect()
}

async fn wait_for(
    client: &RemoteClient,
    cond: impl Fn(&modron_mobile_core::ViewModelSnapshot) -> bool,
) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let snap = client.state();
        if cond(&snap) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting; last error: {:?}",
            snap.last_error
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

/// Connect is fire-and-forget: wait for the background handshake.
async fn wait_connected(client: &RemoteClient) {
    wait_for(client, |snap| snap.connection_label == "connected").await;
}

/// Attach is fire-and-forget: wait for the snapshot fold to land.
async fn wait_attached(client: &RemoteClient, session: Uuid) {
    wait_for(client, |snap| snap.session_id == Some(session)).await;
}

async fn connect_client(url: &str) -> RemoteClient {
    let client = RemoteClient::connect(url, None).expect("connect accepted");
    wait_connected(&client).await;
    client
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn handshake_attach_snapshot_folds_the_full_state() {
    let listener = bind_server().await;
    let url = url_of(&listener).await;
    let session = session_id();
    let events = vec![modron_types::SessionEvent::Task {
        text: "read a.rs".into(),
    }];

    let server = tokio::spawn(async move {
        let mut ws = accept_ws(&listener).await;
        greet(&mut ws).await;
        let attach = read_frame(&mut ws).await;
        match attach.payload {
            Payload::Attach {
                session_id: id,
                last_seq,
            } => {
                assert_eq!(id, session);
                assert_eq!(last_seq, 0, "first attach wants the full replay");
            }
            other => panic!("expected attach, got {other:?}"),
        }
        send_frame(
            &mut ws,
            &snapshot_frame(
                session,
                3,
                events.clone(),
                LiveState {
                    running: true,
                    streaming_text: "partial".into(),
                    streaming_reasoning: "thinking".into(),
                    model: "gpt-5.6-sol".into(),
                    provider: "openai-compat".into(),
                    ..LiveState::default()
                },
            ),
        )
        .await;
        // Phase 2 (after the client opts in and sends the flush marker):
        // a fresh snapshot whose live reasoning buffer now seeds.
        let marker = read_frame(&mut ws).await;
        assert!(matches!(marker.payload, Payload::SetConfig { .. }));
        send_frame(
            &mut ws,
            &snapshot_frame(
                session,
                4,
                events,
                LiveState {
                    running: true,
                    streaming_text: "partial".into(),
                    streaming_reasoning: "thinking".into(),
                    model: "gpt-5.6-sol".into(),
                    provider: "openai-compat".into(),
                    ..LiveState::default()
                },
            ),
        )
        .await;
        // Hold the socket open until the client finishes.
        let _ = ws.next().await;
    });

    let client = connect_client(&url).await;
    assert!(client.is_connected());
    client.attach(session, 0).expect("attach");
    wait_attached(&client, session).await;

    let snap = client.state();
    assert_eq!(snap.session_id, Some(session));
    assert_eq!(snap.connection_label, "connected");
    assert_eq!(snap.server_version, "0.1.2");
    assert!(snap.running);
    assert_eq!(snap.running_label, "running");
    assert_eq!(snap.streaming_text, "partial");
    // Reasoning is opt-in (invariant 5): the snapshot's live reasoning
    // buffer stays hidden while the user is opted out.
    assert!(!snap.show_reasoning);
    assert_eq!(snap.streaming_reasoning, "");
    assert_eq!(snap.model, "gpt-5.6-sol");
    assert_eq!(snap.provider, "openai-compat");
    assert_eq!(snap.status_line, "gpt-5.6-sol · 0 tok · ask");
    // Snapshot replay became history + one drained batch.
    let entries = client.take_new_entries();
    assert_eq!(entries.len(), 1, "the task echo row");
    assert!(matches!(entries[0], TranscriptEntry::Task { .. }));
    assert_eq!(entry_texts(&client), vec!["❯ read a.rs"]);
    assert_eq!(snap.last_seq, 3);

    // Opt in, signal the server (an empty patch doubles as the marker),
    // and the next snapshot's reasoning buffer seeds.
    client.set_show_reasoning(true);
    client
        .set_config(ConfigPatch::default())
        .expect("send the flush marker");
    wait_for(&client, |snap| !snap.streaming_reasoning.is_empty()).await;
    let snap = client.state();
    assert!(snap.show_reasoning);
    assert_eq!(snap.streaming_reasoning, "thinking");
    assert_eq!(snap.streaming_text, "partial");

    server.abort();
}

#[tokio::test]
async fn live_streaming_commits_on_turn_done() {
    let listener = bind_server().await;
    let url = url_of(&listener).await;
    let session = session_id();

    let server = tokio::spawn(async move {
        let mut ws = accept_ws(&listener).await;
        greet(&mut ws).await;
        let _attach = read_frame(&mut ws).await;
        send_frame(
            &mut ws,
            &snapshot_frame(
                session,
                0,
                vec![],
                LiveState {
                    running: true,
                    ..LiveState::default()
                },
            ),
        )
        .await;
        // A live run: task echo, tool activity, streaming text, usage.
        send_frame(
            &mut ws,
            &Frame::event(
                Some(session),
                1,
                Payload::Event {
                    event: modron_types::SessionEvent::Task {
                        text: "count the files".into(),
                    },
                },
            ),
        )
        .await;
        send_frame(
            &mut ws,
            &Frame::event(
                Some(session),
                2,
                Payload::Tool {
                    activity: WireToolActivity::Start {
                        name: "glob".into(),
                    },
                },
            ),
        )
        .await;
        send_frame(&mut ws, &text_delta("There are ", 3, session)).await;
        send_frame(&mut ws, &text_delta("42 files.", 4, session)).await;
        send_frame(
            &mut ws,
            &Frame::event(
                Some(session),
                5,
                Payload::Tool {
                    activity: WireToolActivity::Done {
                        name: "glob".into(),
                        is_error: false,
                        bytes: 2150,
                    },
                },
            ),
        )
        .await;
        send_frame(
            &mut ws,
            &Frame::event(
                Some(session),
                6,
                Payload::Event {
                    event: modron_types::SessionEvent::Usage {
                        model: "m".into(),
                        usage: modron_types::Usage {
                            input_tokens: 75,
                            output_tokens: 25,
                            cached_input_tokens: 50,
                        },
                    },
                },
            ),
        )
        .await;
        // Wait for a client frame (the detach signal) before ending the
        // turn, so the mid-run assertions are deterministic.
        let _signal = read_frame(&mut ws).await;
        send_frame(
            &mut ws,
            &Frame::event(
                Some(session),
                7,
                Payload::TurnDone {
                    summary: modron_protocol::WireRunSummary {
                        session_id: session,
                        answer: "There are 42 files.".into(),
                        turns: 1,
                        tool_calls: 1,
                        usage: modron_types::Usage::default(),
                        models_used: vec!["m".into()],
                    },
                },
            ),
        )
        .await;
        let _ = ws.next().await;
    });

    let client = connect_client(&url).await;
    client.attach(session, 0).expect("attach");
    wait_attached(&client, session).await;
    client.take_new_entries(); // drain the empty snapshot

    // Mid-run, fully deterministic: the task row committed, the tool is
    // done, and the answer is still streaming.
    loop {
        let snap = client.state();
        if snap.streaming_text == "There are 42 files."
            && snap.tool_lines == vec!["✓ glob — 2.1 KB"]
        {
            break;
        }
        client.take_new_entries();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let mid = client.state();
    assert_eq!(
        mid.turn_summary_line.as_deref(),
        Some("⏺ 1 tool · glob"),
        "the live strip's collapsed line, pre-commit"
    );
    assert!(mid.running);
    client
        .detach()
        .expect("detach doubles as the end-turn signal");

    // After TurnDone: text committed exactly once, tool summary landed,
    // view idle.
    loop {
        if !client.state().running {
            break;
        }
        client.take_new_entries();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let texts: Vec<String> = entry_texts(&client);
    assert_eq!(
        texts,
        vec![
            "❯ count the files",
            "100 tok · cache 40%",
            "⏺ 1 tool · glob",
            "There are 42 files.",
        ],
        "task echo, usage note, grouped tool strip at turn end, committed answer"
    );
    let done = client.state();
    assert_eq!(done.streaming_text, "", "buffer cleared at commit");
    assert!(done.tool_lines.is_empty(), "strip cleared at turn end");
    assert_eq!(done.turns_completed, 1);
    assert_eq!(done.tool_calls_total, 1);
    assert_eq!(done.cumulative_tokens, 100);
    assert_eq!(done.session_cache_pct, Some(40.0));
    assert_eq!(done.running_label, "idle");

    server.abort();
}

#[tokio::test]
async fn approval_pending_then_resolved() {
    let listener = bind_server().await;
    let url = url_of(&listener).await;
    let session = session_id();

    let server = tokio::spawn(async move {
        let mut ws = accept_ws(&listener).await;
        greet(&mut ws).await;
        let _attach = read_frame(&mut ws).await;
        send_frame(
            &mut ws,
            &snapshot_frame(session, 0, vec![], LiveState::default()),
        )
        .await;
        send_frame(
            &mut ws,
            &Frame::event(
                Some(session),
                1,
                Payload::ApprovalRequested {
                    request_id: "req-9".into(),
                    call: modron_types::ToolCall {
                        id: "c1".into(),
                        name: "edit".into(),
                        arguments: serde_json::json!({"path": "a.rs", "change": "fix"}),
                    },
                },
            ),
        )
        .await;
        let approve = read_frame(&mut ws).await;
        match approve.payload {
            Payload::Approve {
                request_id,
                decision,
            } => {
                assert_eq!(request_id, "req-9");
                assert_eq!(decision, Decision::AllowOnce);
            }
            other => panic!("expected approve, got {other:?}"),
        }
        send_frame(
            &mut ws,
            &Frame::event(
                Some(session),
                2,
                Payload::ApprovalResolved {
                    request_id: "req-9".into(),
                    decision: Decision::AllowOnce,
                },
            ),
        )
        .await;
        let _ = ws.next().await;
    });

    let client = connect_client(&url).await;
    client.attach(session, 0).expect("attach");
    wait_attached(&client, session).await;

    // The approval surfaces with the shared labels and a one-line summary.
    loop {
        if client.state().pending_approval.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let snap = client.state();
    let approval = snap.pending_approval.as_ref().expect("pending");
    assert_eq!(approval.request_id, "req-9");
    assert_eq!(approval.tool, "edit");
    assert!(approval.args_summary.contains("a.rs"));
    let labels: Vec<String> = approval.options.iter().map(|o| o.label.clone()).collect();
    assert_eq!(labels, vec!["allow once", "allow all", "deny"]);

    let taken = client.take_pending_approval().expect("take drains");
    assert_eq!(taken.request_id, "req-9");
    assert!(
        client.take_pending_approval().is_none(),
        "second take is empty"
    );

    // Approve: the wire echo clears the rest of the state.
    client
        .approve("req-9", Decision::AllowOnce)
        .expect("send approve");
    while let Some(a) = client.state().pending_approval {
        assert_eq!(a.request_id, "req-9");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(client.state().pending_approval.is_none());

    server.abort();
}

#[tokio::test]
async fn interrupt_aborts_and_discards_the_buffer() {
    let listener = bind_server().await;
    let url = url_of(&listener).await;
    let session = session_id();

    let server = tokio::spawn(async move {
        let mut ws = accept_ws(&listener).await;
        greet(&mut ws).await;
        let _attach = read_frame(&mut ws).await;
        send_frame(
            &mut ws,
            &snapshot_frame(
                session,
                0,
                vec![],
                LiveState {
                    running: true,
                    ..LiveState::default()
                },
            ),
        )
        .await;
        send_frame(&mut ws, &text_delta("half a thought", 1, session)).await;
        let interrupt = read_frame(&mut ws).await;
        match interrupt.payload {
            Payload::Interrupt { session_id: id } => assert_eq!(id, session),
            other => panic!("expected interrupt, got {other:?}"),
        }
        send_frame(
            &mut ws,
            &Frame::event(Some(session), 2, Payload::TurnAborted {}),
        )
        .await;
        let _ = ws.next().await;
    });

    let client = connect_client(&url).await;
    client.attach(session, 0).expect("attach");
    wait_attached(&client, session).await;
    assert!(client.state().running);

    loop {
        if client.state().streaming_text == "half a thought" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    client.interrupt(session).expect("send interrupt");
    loop {
        if !client.state().running {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let snap = client.state();
    assert_eq!(snap.streaming_text, "", "aborted buffer discards");
    assert!(snap.last_error.is_none(), "a clean abort is not an error");
    let committed: Vec<String> = entry_texts(&client);
    assert!(
        !committed.iter().any(|t| t.contains("half a thought")),
        "aborted streaming text never commits"
    );

    server.abort();
}

#[tokio::test]
async fn disconnect_then_reconnect_resumes_from_last_seq() {
    let listener = bind_server().await;
    let url = url_of(&listener).await;
    let session = session_id();
    let persisted = vec![
        modron_types::SessionEvent::Task {
            text: "deploy the thing".into(),
        },
        modron_types::SessionEvent::Usage {
            model: "m".into(),
            usage: modron_types::Usage {
                input_tokens: 1000,
                output_tokens: 100,
                cached_input_tokens: 500,
            },
        },
    ];

    let server = tokio::spawn(async move {
        // First connection: handshake, attach, snapshot(seq 2), then drop.
        let mut ws = accept_ws(&listener).await;
        greet(&mut ws).await;
        let _attach = read_frame(&mut ws).await;
        send_frame(
            &mut ws,
            &snapshot_frame(
                session,
                2,
                persisted.clone(),
                LiveState {
                    cumulative_usage: modron_types::Usage {
                        input_tokens: 1000,
                        output_tokens: 100,
                        cached_input_tokens: 500,
                    },
                    ..LiveState::default()
                },
            ),
        )
        .await;
        // The client waits for the close; drop the socket abruptly.
        drop(ws);

        // Second connection: the client re-attaches from its last seq and
        // receives the full replayable log (persisted + missed events).
        let mut ws = accept_ws(&listener).await;
        greet(&mut ws).await;
        let attach = read_frame(&mut ws).await;
        match attach.payload {
            Payload::Attach { last_seq, .. } => {
                assert_eq!(last_seq, 2, "resume from the seq seen before the drop");
            }
            other => panic!("expected attach, got {other:?}"),
        }
        let mut full_log = persisted;
        full_log.push(modron_types::SessionEvent::Usage {
            model: "m".into(),
            usage: modron_types::Usage {
                input_tokens: 10,
                output_tokens: 5,
                cached_input_tokens: 0,
            },
        });
        send_frame(
            &mut ws,
            &snapshot_frame(
                session,
                5,
                full_log,
                LiveState {
                    cumulative_usage: modron_types::Usage {
                        input_tokens: 1010,
                        output_tokens: 105,
                        cached_input_tokens: 500,
                    },
                    ..LiveState::default()
                },
            ),
        )
        .await;
        let _ = ws.next().await;
    });

    let client = connect_client(&url).await;
    client.attach(session, 0).expect("first attach");
    wait_attached(&client, session).await;
    assert_eq!(client.state().cumulative_tokens, 1100);
    assert_eq!(client.take_new_entries().len(), 2, "task + usage rows");

    client.disconnect();
    client.wait_closed().await;
    let snap = client.state();
    assert_eq!(snap.connection_label, "disconnected");
    assert_eq!(snap.last_seq, 2, "seq advanced past the snapshot");

    client.reconnect(&url, session, None).expect("reconnect");
    wait_connected(&client).await;
    wait_attached(&client, session).await;
    assert_eq!(client.state().connection_label, "connected");
    assert_eq!(client.state().cumulative_tokens, 1115, "no double count");
    let texts = entry_texts(&client);
    assert_eq!(texts.len(), 3, "snapshot replay rebuilt the transcript");
    assert_eq!(client.take_new_entries().len(), 3, "full replay batch");
    assert!(
        texts[2].starts_with("15 tok"),
        "missed usage note: {texts:?}"
    );

    server.abort();
}

#[tokio::test]
async fn protocol_mismatch_fails_the_handshake() {
    let listener = bind_server().await;
    let url = url_of(&listener).await;
    let server = tokio::spawn(async move {
        let mut ws = accept_ws(&listener).await;
        let _hello = read_frame(&mut ws).await;
        send_frame(
            &mut ws,
            &Frame::event(
                None,
                0,
                Payload::Welcome {
                    server_version: "future".into(),
                    protocol: PROTOCOL_VERSION + 1,
                },
            ),
        )
        .await;
        let _ = ws.next().await;
    });

    // The mismatch surfaces asynchronously now: the background handshake
    // fails into the state instead of the connect call.
    let client = RemoteClient::connect(&url, None).expect("connect accepted");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let snap = client.state();
        if let Some(err) = &snap.last_error {
            assert!(err.contains("protocol mismatch"), "{err}");
            assert!(err.contains("server v2, client v1"), "{err}");
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for the mismatch"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    server.abort();
}

#[tokio::test]
async fn config_changed_updates_labels_and_list_sessions_folds() {
    let listener = bind_server().await;
    let url = url_of(&listener).await;
    let session = session_id();

    let server = tokio::spawn(async move {
        let mut ws = accept_ws(&listener).await;
        greet(&mut ws).await;
        let _attach = read_frame(&mut ws).await;
        send_frame(
            &mut ws,
            &snapshot_frame(session, 0, vec![], LiveState::default()),
        )
        .await;
        // Answer list_sessions with two rows.
        let list = read_frame(&mut ws).await;
        match list.payload {
            Payload::ListSessions {} => {}
            other => panic!("expected list_sessions, got {other:?}"),
        }
        send_frame(
            &mut ws,
            &Frame::request(
                list.id.expect("correlated reply carries the id"),
                Payload::Sessions {
                    sessions: vec![modron_protocol::SessionMetaWire {
                        task_branch: None,
                        task_state: None,
                        working_dir: None,
                        id: session,
                        title: "deploy the thing".into(),
                        created_at: "2026-11-09T00:00:00Z".into(),
                        updated_at: "2026-11-09T01:00:00Z".into(),
                        message_count: 4,
                    }],
                },
            ),
        )
        .await;
        // Model switch parity: the patch sends the model only; the server
        // follows the provider through the catalog (2026-11-12 audit fix).
        let set_config = read_frame(&mut ws).await;
        match set_config.payload {
            Payload::SetConfig { patch } => {
                assert_eq!(patch.model.as_deref(), Some("claude-opus-5"));
                assert_eq!(
                    patch.provider, None,
                    "the client must not pin the stale provider"
                );
            }
            other => panic!("expected set_config, got {other:?}"),
        }
        send_frame(
            &mut ws,
            &Frame::event(
                None,
                0,
                Payload::ConfigChanged {
                    patch: ConfigPatch {
                        model: Some("claude-opus-5".into()),
                        provider: Some("anthropic".into()),
                        approval_mode: Some(modron_protocol::ApprovalModeWire::ApproveAll),
                        ..ConfigPatch::default()
                    },
                },
            ),
        )
        .await;
        let _ = ws.next().await;
    });

    let client = connect_client(&url).await;
    client.attach(session, 0).expect("attach");
    wait_attached(&client, session).await;

    client.list_sessions().expect("send list");
    wait_for(&client, |snap| !snap.sessions.is_empty()).await;
    let sessions = client.state().sessions;
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].title, "deploy the thing");

    client
        .select_model("claude-opus-5")
        .expect("send select_model");
    loop {
        if client.state().model == "claude-opus-5" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let snap = client.state();
    assert_eq!(snap.provider, "anthropic");
    assert_eq!(snap.approval_mode_label, "yolo");
    assert!(snap.status_line.ends_with(" · yolo"));

    server.abort();
}

#[tokio::test]
async fn commands_after_disconnect_fail_with_closed() {
    let listener = bind_server().await;
    let url = url_of(&listener).await;
    let session = session_id();
    let server = tokio::spawn(async move {
        let mut ws = accept_ws(&listener).await;
        greet(&mut ws).await;
        // Let the client finish its background handshake before the drop:
        // an instant RST can otherwise outrun the welcome read.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(ws); // close abruptly: the run loop sees the reset
    });

    let client = connect_client(&url).await;
    client.wait_closed().await;
    let err = client.submit(Some(session), "hi").expect_err("closed");
    assert!(matches!(err, ClientError::Closed), "{err}");
    let cause = client.state().last_error.expect("cause recorded");
    assert!(
        cause.contains("closed") || cause.contains("reset"),
        "cause recorded: {cause}"
    );
    server.abort();
}

#[tokio::test]
async fn snapshot_seeds_live_tools_and_subagents() {
    let listener = bind_server().await;
    let url = url_of(&listener).await;
    let session = session_id();
    let server = tokio::spawn(async move {
        let mut ws = accept_ws(&listener).await;
        greet(&mut ws).await;
        let _attach = read_frame(&mut ws).await;
        send_frame(
            &mut ws,
            &snapshot_frame(
                session,
                1,
                vec![],
                LiveState {
                    running: true,
                    turn_tools: vec![ToolEvent {
                        name: "read".into(),
                        bytes: Some(64),
                        is_error: false,
                        done: true,
                    }],
                    subagents: vec![modron_protocol::WireSwarmRow {
                        id: "a1".into(),
                        label: "explore".into(),
                        turns: 2,
                        tokens: 500,
                        last_tool: Some("grep".into()),
                        done: None,
                    }],
                    ..LiveState::default()
                },
            ),
        )
        .await;
        let _ = ws.next().await;
    });

    let client = connect_client(&url).await;
    client.attach(session, 0).expect("attach");
    wait_attached(&client, session).await;
    let snap = client.state();
    assert_eq!(snap.tool_lines, vec!["✓ read — 64 B"]);
    assert_eq!(
        snap.subagent_rows,
        vec!["⏵ [explore] running · 2 turns · 500 tok · last: grep"]
    );
    assert_eq!(snap.turn_summary_line.as_deref(), Some("⏺ 1 tool · read"));

    server.abort();
}

// The share URL surfaces in the note (desktop parity): the TSX renders
// last_note in the chat scrollback, so /remote-control can show it.
#[tokio::test]
async fn remote_control_started_lands_in_the_note() {
    let listener = bind_server().await;
    let url = url_of(&listener).await;
    let session = session_id();
    let share_url = "https://bridge.example.com/s/xyz";
    let server = tokio::spawn(async move {
        let mut ws = accept_ws(&listener).await;
        greet(&mut ws).await;
        let _attach = read_frame(&mut ws).await;
        send_frame(
            &mut ws,
            &snapshot_frame(session, 0, vec![], LiveState::default()),
        )
        .await;
        send_frame(
            &mut ws,
            &Frame::event(
                Some(session),
                1,
                Payload::RemoteControlStarted {
                    url: share_url.into(),
                },
            ),
        )
        .await;
        let _ = ws.next().await;
    });

    let client = connect_client(&url).await;
    client.attach(session, 0).expect("attach");
    wait_attached(&client, session).await;
    wait_for(&client, |snap| snap.last_note.is_some()).await;
    let note = client.state().last_note.expect("note");
    assert!(
        note.contains("remote-control live")
            && note.contains(share_url)
            && note.contains("open on any device"),
        "note names the share URL: {note}"
    );

    server.abort();
}

// Parity invariant 5 end to end: reasoning deltas are dropped while
// show_reasoning is off (the default) and stream once opted in; the
// snapshot-seeded live reasoning buffer follows the same gate.
#[tokio::test]
async fn reasoning_deltas_stream_only_when_opted_in() {
    let listener = bind_server().await;
    let url = url_of(&listener).await;
    let session = session_id();
    let server = tokio::spawn(async move {
        let mut ws = accept_ws(&listener).await;
        greet(&mut ws).await;
        let _attach = read_frame(&mut ws).await;
        // A live half already carrying reasoning: must NOT surface while
        // the user is opted out.
        send_frame(
            &mut ws,
            &snapshot_frame(
                session,
                0,
                vec![],
                LiveState {
                    running: true,
                    streaming_reasoning: "seeded thought".into(),
                    ..LiveState::default()
                },
            ),
        )
        .await;
        send_frame(
            &mut ws,
            &Frame::event(
                Some(session),
                1,
                Payload::Delta {
                    delta: WireDelta::Reasoning {
                        text: "dropped ".into(),
                    },
                },
            ),
        )
        .await;
        // Wait for the opt-in to apply before streaming more, so the
        // assertions are deterministic.
        loop {
            let frame = read_frame(&mut ws).await;
            if matches!(frame.payload, Payload::SetConfig { .. }) {
                break;
            }
        }
        send_frame(
            &mut ws,
            &Frame::event(
                Some(session),
                2,
                Payload::Delta {
                    delta: WireDelta::Reasoning {
                        text: "kept".into(),
                    },
                },
            ),
        )
        .await;
        let _ = ws.next().await;
    });

    let client = connect_client(&url).await;
    client.attach(session, 0).expect("attach");
    wait_attached(&client, session).await;
    // Give the dropped delta time to arrive (it must not surface).
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let snap = client.state();
    assert!(!snap.show_reasoning, "default off");
    assert_eq!(
        snap.streaming_reasoning, "",
        "snapshot seed and live deltas stay hidden while opted out"
    );

    // Opt in (no wire payload for it — a local preference), the live
    // buffer is still empty because the earlier delta was dropped.
    client.set_show_reasoning(true);
    // An empty config patch doubles as the flush marker: it tells the
    // scripted server the opt-in above has applied (the pattern the
    // detach-as-signal test uses), so the next delta must land.
    client
        .set_config(ConfigPatch::default())
        .expect("send the flush marker");
    loop {
        if client.state().streaming_reasoning == "kept" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(client.state().show_reasoning);

    server.abort();
}
