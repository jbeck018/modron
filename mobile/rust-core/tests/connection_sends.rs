//! Outbound frames on a live connection must reach the server, whatever
//! the inbound traffic before the first command was (W6, FINDINGS F13).
//! Each variant drives inbound frames first, then sends a command and
//! keepalive traffic, and asserts that the server reads them.

use futures_util::{SinkExt, StreamExt};
use modron_mobile_core::{ConnectOptions, KeepaliveConfig, RemoteClient};
use modron_protocol::{
    ErrorKind, Frame, LiveState, Payload, WireDelta, WireToolActivity, PROTOCOL_VERSION,
};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{accept_async, WebSocketStream};
use uuid::Uuid;

type Ws = WebSocketStream<TcpStream>;

fn options() -> ConnectOptions {
    ConnectOptions {
        keepalive: KeepaliveConfig {
            ping_every: Duration::from_millis(50),
            idle_timeout: Duration::from_secs(10),
        },
        ..ConnectOptions::default()
    }
}

async fn accept_ws(listener: &TcpListener) -> Ws {
    let (stream, _) = listener.accept().await.expect("accept");
    accept_async(stream).await.expect("ws accept")
}

/// Next client text frame within `dur`; `None` on timeout or close.
async fn next_client_frame(ws: &mut Ws, dur: Duration) -> Option<Frame> {
    loop {
        match tokio::time::timeout(dur, ws.next()).await {
            Err(_) | Ok(None) | Ok(Some(Err(_))) | Ok(Some(Ok(WsMessage::Close(_)))) => {
                return None
            }
            Ok(Some(Ok(WsMessage::Text(json)))) => {
                return Some(serde_json::from_str(&json).expect("client frame json"))
            }
            Ok(Some(Ok(_))) => continue,
        }
    }
}

async fn send(ws: &mut Ws, frame: &Frame) {
    let json = serde_json::to_string(frame).expect("encode");
    ws.send(WsMessage::Text(json.into()))
        .await
        .expect("server send");
}

async fn greet(ws: &mut Ws) {
    let hello = next_client_frame(ws, Duration::from_secs(5))
        .await
        .expect("hello");
    assert!(matches!(hello.payload, Payload::Hello { .. }));
    send(
        ws,
        &Frame::event(
            None,
            0,
            Payload::Welcome {
                server_version: "0.1.2".into(),
                protocol: PROTOCOL_VERSION,
                commands: Vec::new(),
            },
        ),
    )
    .await;
}

fn snapshot(session: Uuid, events: Vec<modron_types::SessionEvent>, running: bool) -> Frame {
    Frame::event(
        Some(session),
        0,
        Payload::Snapshot {
            session_id: session,
            last_seq: 0,
            events,
            live: LiveState {
                running,
                ..LiveState::default()
            },
        },
    )
}

/// The frames of one run, as a live client sees them before it sends.
fn run_frames(session: Uuid) -> Vec<Frame> {
    vec![
        Frame::event(
            Some(session),
            1,
            Payload::Event {
                event: modron_types::SessionEvent::Task {
                    text: "count the files".into(),
                },
            },
        ),
        Frame::event(
            Some(session),
            2,
            Payload::Delta {
                delta: WireDelta::Text {
                    text: "Counting".into(),
                },
            },
        ),
        Frame::event(
            Some(session),
            3,
            Payload::Tool {
                activity: WireToolActivity::Start {
                    name: "glob".into(),
                    call_id: None,
                },
            },
        ),
        Frame::event(
            Some(session),
            4,
            Payload::ApprovalRequested {
                request_id: "req-1".into(),
                call: modron_types::ToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command": "ls"}),
                },
                agent: None,
            },
        ),
        Frame::event(
            Some(session),
            5,
            Payload::ApprovalResolved {
                request_id: "req-1".into(),
                decision: modron_protocol::Decision::AllowOnce,
            },
        ),
        Frame::event(
            Some(session),
            6,
            Payload::TurnDone {
                summary: modron_protocol::WireRunSummary {
                    session_id: session,
                    answer: "There are 42 files.".into(),
                    turns: 1,
                    tool_calls: 1,
                    usage: modron_types::Usage::default(),
                    models_used: vec![],
                },
            },
        ),
    ]
}

/// Shared script: wait for the client to send `compact`, then
/// `send_message`, then count keepalive pings. Panics with the variant
/// name when a frame never arrives.
async fn expect_commands_and_pings(ws: &mut Ws, session: Uuid, who: &str) {
    let dur = Duration::from_secs(3);
    let mut saw_compact = false;
    let mut saw_send = false;
    let mut pings = 0;
    while !(saw_compact && saw_send && pings >= 3) {
        let Some(frame) = next_client_frame(ws, dur).await else {
            panic!(
                "{who}: no frame reached the server (compact={saw_compact} send={saw_send} pings={pings})"
            );
        };
        match frame.payload {
            Payload::Compact { session_id } => {
                assert_eq!(session_id, session);
                saw_compact = true;
            }
            Payload::Submit { .. } => saw_send = true,
            Payload::Ping {} => pings += 1,
            _ => {}
        }
    }
}

async fn setup(url: &str) -> RemoteClient {
    let client = RemoteClient::connect_with_options(url, None, options()).expect("connect");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while client.state().connection_label != "connected" {
        assert!(tokio::time::Instant::now() < deadline, "never connected");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    client
}

async fn run_variant(who: &str, session: Uuid, snap: Frame, extra: Vec<Frame>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("ws://{}", listener.local_addr().expect("addr"));
    let mut snap = Some(snap);
    let server_who = who.to_string();
    let server = tokio::spawn(async move {
        let mut ws = accept_ws(&listener).await;
        greet(&mut ws).await;
        // The attach comes first on a reconnect-less client; no attach
        // here: the page's first command is the compact under test.
        send(&mut ws, &snap.take().expect("snapshot")).await;
        for frame in &extra {
            send(&mut ws, frame).await;
        }
        expect_commands_and_pings(&mut ws, session, &server_who).await;
    });
    let client = setup(&url).await;
    // Let inbound traffic land before the first command.
    tokio::time::sleep(Duration::from_millis(100)).await;
    client.attach(session, 0).expect("attach");
    tokio::time::sleep(Duration::from_millis(100)).await;
    client
        .compact()
        .expect("compact returns Ok while connected");
    client.send_message("hello").expect("send_message");
    server.await.expect("server script");
}

#[tokio::test]
async fn run_frames_before_the_first_command_do_not_stop_later_sends() {
    let session = Uuid::new_v4();
    run_variant(
        "run frames",
        session,
        snapshot(session, vec![], false),
        run_frames(session),
    )
    .await;
}

#[tokio::test]
async fn snapshot_over_200_kb_does_not_stop_later_sends() {
    let session = Uuid::new_v4();
    let big = "x".repeat(260 * 1024);
    let snap = snapshot(
        session,
        vec![modron_types::SessionEvent::Task { text: big }],
        false,
    );
    run_variant("large snapshot", session, snap, vec![]).await;
}

#[tokio::test]
async fn error_frame_does_not_stop_later_sends() {
    let session = Uuid::new_v4();
    run_variant(
        "error frame",
        session,
        snapshot(session, vec![], false),
        vec![Frame::event(
            Some(session),
            1,
            Payload::Error {
                message: "compaction refused".into(),
                kind: Some(ErrorKind::Forbidden),
            },
        )],
    )
    .await;
}

async fn wait_disconnected(client: &RemoteClient) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while client.state().connection_label == "connected" {
        assert!(tokio::time::Instant::now() < deadline, "still connected");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn a_send_on_a_dead_connection_is_an_error_and_says_why() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("ws://{}", listener.local_addr().expect("addr"));
    let session = Uuid::new_v4();
    let server = tokio::spawn(async move {
        let mut ws = accept_ws(&listener).await;
        greet(&mut ws).await;
        let attach = next_client_frame(&mut ws, Duration::from_secs(5))
            .await
            .expect("attach");
        assert!(matches!(attach.payload, Payload::Attach { .. }));
        send(&mut ws, &snapshot(session, vec![], false)).await;
        // The server goes away: the socket closes under the client.
        drop(ws);
    });
    let client = setup(&url).await;
    client.attach(session, 0).expect("attach while connected");
    server.await.expect("server script");
    wait_disconnected(&client).await;

    let err = client
        .compact()
        .expect_err("a send on a dead connection must be an error");
    assert!(
        matches!(err, modron_mobile_core::ClientError::Closed),
        "unexpected error: {err:?}"
    );
    let last = client.state().last_error.expect("the refusal is surfaced");
    assert!(
        last.contains("connection closed") || last.contains("closed"),
        "{last}"
    );
}

#[tokio::test]
async fn a_send_after_reconnect_reaches_the_new_socket() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("ws://{}", listener.local_addr().expect("addr"));
    let session = Uuid::new_v4();
    let server = tokio::spawn(async move {
        // First socket: greet, take the attach, then drop it.
        let mut ws = accept_ws(&listener).await;
        greet(&mut ws).await;
        let _attach = next_client_frame(&mut ws, Duration::from_secs(5))
            .await
            .expect("attach");
        send(&mut ws, &snapshot(session, vec![], false)).await;
        drop(ws);
        // Second socket: the reconnect re-attaches, then the compact must arrive.
        let mut ws = accept_ws(&listener).await;
        greet(&mut ws).await;
        let reattach = next_client_frame(&mut ws, Duration::from_secs(5))
            .await
            .expect("reattach");
        assert!(matches!(reattach.payload, Payload::Attach { .. }));
        send(&mut ws, &snapshot(session, vec![], false)).await;
        expect_commands_and_pings(&mut ws, session, "after reconnect").await;
    });
    let client = setup(&url).await;
    client.attach(session, 0).expect("attach");
    wait_disconnected(&client).await;
    client
        .reconnect(&url, session, None)
        .expect("reconnect accepted");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while client.state().connection_label != "connected" {
        assert!(
            tokio::time::Instant::now() < deadline,
            "reconnect never connected"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    client.compact().expect("compact after reconnect");
    client
        .send_message("hello")
        .expect("send_message after reconnect");
    server.await.expect("server script");
}
