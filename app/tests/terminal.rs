//! Terminal host startup and lifetime. The server tests run the real
//! `modron serve` binary when it is present at [`BIN`] and skip otherwise;
//! the reconnect test uses an in-process scripted server.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc,
    time::{Duration, Instant},
};

use futures_util::{SinkExt, StreamExt};
use modron_app::{
    chat::UiAction,
    session::{self, LiveOptions, Resume},
    startup::{self, Args, Plan, ServerChild, ServerSpec, Source},
};
use modron_mobile_core::ConnectionState;
use modron_protocol::{Frame, LiveState, Payload, PROTOCOL_VERSION};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{accept_async, WebSocketStream};
use uuid::Uuid;

const BIN: &str = "/tmp/claude-0/e2e/modron";
const HOME: &str = "/tmp/claude-0/c7/h";
const REPO: &str = "/tmp/claude-0/c7/repo";
const WAIT: Duration = Duration::from_secs(20);

/// The prebuilt server, when this machine has it.
fn server_bin() -> Option<PathBuf> {
    let bin = PathBuf::from(BIN);
    if bin.is_file() {
        Some(bin)
    } else {
        eprintln!("skipped: {BIN} is absent");
        None
    }
}

/// A short HOME and a scratch git repo for the server to run in.
fn scratch() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        fs::create_dir_all(HOME).expect("home dir");
        fs::create_dir_all(REPO).expect("repo dir");
        if !Path::new(REPO).join(".git").exists() {
            let status = Command::new("git").args(["init", "-q", REPO]).status().expect("git");
            assert!(status.success(), "git init failed");
        }
    });
}

/// Pumps snapshots until the driver reports `connected`.
fn wait_connected(live: &mut session::Live) {
    let deadline = Instant::now() + WAIT;
    loop {
        assert!(Instant::now() < deadline, "driver never connected");
        if live.try_snapshot().is_some_and(|s| s.connection == ConnectionState::Connected) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("addr").port()
}

/// True while the process exists (zombies were reaped by the test's drop).
fn alive(pid: u32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn spec(bin: PathBuf, listen: &str) -> ServerSpec {
    ServerSpec {
        bin,
        listen: listen.into(),
        provider: Some("mock".into()),
        model: None,
        log: PathBuf::from(HOME).join("serve.log"),
        dir: Some(REPO.into()),
        home: Some(HOME.into()),
    }
}

#[test]
fn owned_server_is_started_connected_and_stopped_on_exit() {
    let Some(bin) = server_bin() else { return };
    scratch();
    let listen = format!("127.0.0.1:{}", free_port());
    let url = format!("ws://{listen}");
    let server =
        ServerChild::start(&spec(bin, &listen), Duration::from_secs(10)).expect("server starts");
    let pid = server.pid();
    assert!(alive(pid), "the server child is running");

    let mut live =
        session::spawn(LiveOptions { url: url.clone(), ..Default::default() }).expect("driver");
    wait_connected(&mut live);
    live.act(UiAction::Send("hello from the terminal host".into()));
    let deadline = Instant::now() + WAIT;
    let mut seen = None;
    while seen.is_none() {
        assert!(Instant::now() < deadline, "no streamed reply from the server");
        if let Some(s) = live.try_snapshot() {
            let streamed = s.session_id.is_some()
                && s.connection == ConnectionState::Connected
                && (!s.streaming_text.is_empty() || s.history.len() >= 2);
            if streamed {
                seen = Some(s);
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let snapshot = seen.expect("snapshot");
    assert!(snapshot.connection_label.contains(&url), "{}", snapshot.connection_label);

    drop(live);
    drop(server);
    assert!(!alive(pid), "the app's server must be gone after exit");
}

#[test]
fn a_running_server_is_reused_and_not_stopped() {
    let Some(bin) = server_bin() else { return };
    scratch();
    let listen = format!("127.0.0.1:{}", free_port());
    let url = format!("ws://{listen}");
    // Started outside the app: the app must find it and take no ownership.
    let external = ServerChild::start(&spec(bin.clone(), &listen), Duration::from_secs(10))
        .expect("external server starts");
    let pid = external.pid();

    let plan = startup::decide(&Args::default(), Some(&url), startup::probe_ws, || {
        panic!("a running server must not be looked up as a binary")
    });
    assert_eq!(plan, Plan::Connect { url: url.clone(), source: Source::Running });

    let mut live = session::spawn(LiveOptions { url, ..Default::default() }).expect("driver");
    let deadline = Instant::now() + WAIT;
    loop {
        assert!(Instant::now() < deadline, "driver never connected");
        if live.try_snapshot().is_some_and(|s| s.connection == ConnectionState::Connected) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(live);
    std::thread::sleep(Duration::from_millis(200));
    assert!(alive(pid), "the running server must survive the app's exit");
    drop(external);
}

type Ws = WebSocketStream<TcpStream>;

async fn read_frame(ws: &mut Ws) -> Frame {
    loop {
        match ws.next().await.expect("server: socket closed").expect("server: read") {
            WsMessage::Text(json) => return serde_json::from_str(&json).expect("server: json"),
            WsMessage::Ping(_) | WsMessage::Pong(_) => continue,
            other => panic!("server: unexpected frame {other:?}"),
        }
    }
}

async fn send_frame(ws: &mut Ws, frame: &Frame) {
    let json = serde_json::to_string(frame).expect("server: encode");
    ws.send(WsMessage::Text(json.into())).await.expect("server: send");
}

/// Two connections. The first attaches, then the socket drops. The second
/// must start with an attach to the same session again. Each attach the
/// server receives is reported.
fn start_flaky_server(session: Uuid) -> (String, mpsc::Receiver<Payload>) {
    let (url_tx, url_rx) = mpsc::channel();
    let (report_tx, report_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("server runtime");
        runtime.block_on(async move {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("addr");
            url_tx.send(format!("ws://{addr}")).expect("send url");
            for round in 0..2 {
                let (stream, _) = listener.accept().await.expect("accept");
                let mut ws = accept_async(stream).await.expect("ws accept");
                let hello = read_frame(&mut ws).await;
                assert!(matches!(hello.payload, Payload::Hello { .. }), "expected hello");
                send_frame(
                    &mut ws,
                    &Frame::event(
                        None,
                        0,
                        Payload::Welcome {
                            server_version: "0.1.3".into(),
                            protocol: PROTOCOL_VERSION,
                        },
                    ),
                )
                .await;
                let attach = read_frame(&mut ws).await;
                report_tx.send(attach.payload).expect("report attach");
                let live = LiveState::default();
                send_frame(
                    &mut ws,
                    &Frame::event(
                        Some(session),
                        0,
                        Payload::Snapshot {
                            session_id: session,
                            last_seq: 0,
                            events: vec![],
                            live,
                        },
                    ),
                )
                .await;
                if round == 0 {
                    // Drop the socket: the client must come back on its own.
                    drop(ws);
                } else {
                    while ws.next().await.is_some() {}
                }
            }
        });
    });
    let url = url_rx.recv_timeout(WAIT).expect("server url");
    (url, report_rx)
}

#[test]
fn dropped_connection_reattaches_to_the_same_session() {
    let session = Uuid::new_v4();
    let (url, attaches) = start_flaky_server(session);
    let mut live = session::spawn(LiveOptions {
        url,
        session: Some(session),
        resume: Resume::New,
        ..Default::default()
    })
    .expect("driver");

    let deadline = Instant::now() + WAIT;
    let mut saw_down = false;
    let mut attached = Vec::new();
    while attached.len() < 2 {
        assert!(Instant::now() < deadline, "no second attach; got {attached:?}");
        while let Some(s) = live.try_snapshot() {
            saw_down |= s.connection == ConnectionState::Disconnected;
        }
        if let Ok(payload) = attaches.try_recv() {
            attached.push(payload);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(saw_down, "the drop must show the disconnected state");
    for payload in &attached {
        assert!(
            matches!(payload, Payload::Attach { session_id, .. } if *session_id == session),
            "expected attach to {session}, got {payload:?}"
        );
    }
}
