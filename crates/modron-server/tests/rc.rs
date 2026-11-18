//! Remote-control tunnel scope tests (B2): share keys are full 122-bit
//! UUIDs, and a tunnel connection (a scoped `ConnCtx`) can only act on
//! its one session. Frames are driven straight through `handle_frame`.

use std::sync::Arc;
use std::time::Duration;

use modron_core::{Engine, EngineConfig, SessionId, SessionStore, ToolCall};
use modron_protocol::{Decision, Frame, Payload};
use modron_server::dispatch::{handle_frame, Outbound};
use modron_server::hub::LiveFrame;
use modron_server::tasks::TaskRegistry;
use modron_server::{BroadcastingStore, ConnCtx, Flow, ServerState, SessionHub};
use tokio::sync::mpsc;

/// A server state over an in-memory store, mirroring `bind_server`'s
/// wiring (the hub's engine template stores through the broadcaster).
fn test_state() -> Arc<ServerState> {
    let hub = SessionHub::empty();
    let inner: Arc<dyn SessionStore> = Arc::new(modron_core::testing::InMemoryStore::new());
    let broadcasting = Arc::new(BroadcastingStore::new(Arc::clone(&inner), hub.clone()));
    let engine = Engine {
        selector: Arc::new(modron_core::testing::StaticSelector::new(
            "mock-model",
            "mock",
        )),
        providers: Default::default(),
        tools: Arc::new(modron_core::ToolRegistry::new()),
        store: Arc::clone(&broadcasting) as Arc<dyn SessionStore>,
        config: EngineConfig {
            default_model: "mock-model".into(),
            default_provider: "mock".into(),
            ..EngineConfig::default()
        },
    };
    hub.set_engine(engine);
    let dir = std::env::temp_dir().join(format!("modron-rc-test-{}", uuid::Uuid::new_v4()));
    Arc::new(ServerState {
        hub,
        store: broadcasting as Arc<dyn SessionStore>,
        devices_dir: dir.join("clients"),
        version: "test".into(),
        working_dir: dir,
        tasks: TaskRegistry::new(),
        bridge_base: None,
        rc_keys: Default::default(),
        portal_exiting: Default::default(),
        rc_tunnels: Default::default(),
    })
}

/// A tunnel-style connection pinned to `session`, plus its outbound queue.
fn scoped_ctx(session: SessionId) -> (ConnCtx, mpsc::UnboundedReceiver<Outbound>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (ConnCtx::scoped(tx, session), rx)
}

/// Next per-connection reply frame (skips broadcasts).
async fn next_reply(rx: &mut mpsc::UnboundedReceiver<Outbound>) -> Frame {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
            Ok(Some(Outbound::Reply(frame))) => return *frame,
            Ok(Some(Outbound::Live(_))) => continue,
            other => panic!("expected a reply frame, got {other:?}"),
        }
    }
}

/// Build a client payload from its wire JSON (avoids depending on every
/// variant's Rust field types here).
fn payload(value: serde_json::Value) -> Payload {
    serde_json::from_value(value).expect("valid client payload")
}

fn error_kind_of(frame: &Frame) -> Option<modron_protocol::ErrorKind> {
    match &frame.payload {
        Payload::Error { kind, .. } => *kind,
        other => panic!("expected an error frame, got {other:?}"),
    }
}

#[test]
fn share_key_is_the_full_122_bit_uuid() {
    let key = modron_server::rc::share_key();
    assert_eq!(key.len(), 32, "share key must be 32 hex chars: {key}");
    assert!(key
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    assert_ne!(key, modron_server::rc::share_key(), "keys must not repeat");
}

#[tokio::test]
async fn scoped_ctx_forbids_server_wide_frames() {
    let state = test_state();
    let session = uuid::Uuid::new_v4();
    state.hub.ensure_session(session, Some(session));
    let (mut ctx, mut rx) = scoped_ctx(session);

    let denied = [
        payload(serde_json::json!({"type": "set_config", "patch": {}})),
        payload(serde_json::json!({
            "type": "auth_set_key", "session_id": session,
            "provider": "anthropic", "key": "k"
        })),
        payload(serde_json::json!({
            "type": "automation_add", "name": "n", "prompt": "p", "interval_minutes": 30
        })),
        payload(serde_json::json!({"type": "delete_session", "session_id": session})),
        payload(serde_json::json!({"type": "list_sessions"})),
        payload(serde_json::json!({"type": "fork", "session_id": session})),
        payload(serde_json::json!({"type": "new_session"})),
        payload(serde_json::json!({"type": "automations_list"})),
    ];
    for (i, frame_payload) in denied.into_iter().enumerate() {
        let flow = handle_frame(
            &state,
            &mut ctx,
            true,
            Frame::request(i as u64 + 1, frame_payload),
        )
        .await;
        assert!(
            flow == Flow::Continue,
            "a denied frame must not close the tunnel"
        );
        let reply = next_reply(&mut rx).await;
        assert_eq!(
            error_kind_of(&reply),
            Some(modron_protocol::ErrorKind::Forbidden),
            "frame {i} must be forbidden"
        );
    }
}

#[tokio::test]
async fn scoped_attach_to_another_session_is_refused() {
    let state = test_state();
    // Attach reads the store, so the scoped session must exist there.
    let own = state.store.create_session("own").await.unwrap().id;
    let other = state.store.create_session("other").await.unwrap().id;
    state.hub.ensure_session(own, Some(own));
    state.hub.ensure_session(other, Some(other));
    let (mut ctx, mut rx) = scoped_ctx(own);

    let flow = handle_frame(
        &state,
        &mut ctx,
        true,
        Frame::request(
            1,
            payload(serde_json::json!({"type": "attach", "session_id": other, "last_seq": 0})),
        ),
    )
    .await;
    assert!(flow == Flow::Continue);
    assert_eq!(
        error_kind_of(&next_reply(&mut rx).await),
        Some(modron_protocol::ErrorKind::Forbidden)
    );
    assert!(
        ctx.subscription.is_none(),
        "a refused attach must not subscribe"
    );

    // Attaching the scoped session itself is allowed.
    handle_frame(
        &state,
        &mut ctx,
        true,
        Frame::request(
            2,
            payload(serde_json::json!({"type": "attach", "session_id": own, "last_seq": 0})),
        ),
    )
    .await;
    let snapshot = next_reply(&mut rx).await;
    assert!(
        matches!(snapshot.payload, Payload::Snapshot { .. }),
        "own attach answers a snapshot"
    );
    assert!(ctx.subscription.is_some());
}

#[tokio::test]
async fn scoped_submit_cannot_target_or_create_another_session() {
    let state = test_state();
    let own = uuid::Uuid::new_v4();
    let other = uuid::Uuid::new_v4();
    state.hub.ensure_session(own, Some(own));
    state.hub.ensure_session(other, Some(other));
    let (mut ctx, mut rx) = scoped_ctx(own);
    let before = state.store.list().await.unwrap().len();

    for session_id in [serde_json::json!(other), serde_json::Value::Null] {
        let flow = handle_frame(
            &state,
            &mut ctx,
            true,
            Frame::request(
                7,
                payload(serde_json::json!({
                    "type": "submit", "session_id": session_id, "task": "do it"
                })),
            ),
        )
        .await;
        assert!(flow == Flow::Continue);
        assert_eq!(
            error_kind_of(&next_reply(&mut rx).await),
            Some(modron_protocol::ErrorKind::Forbidden),
            "submit to {session_id} must be forbidden"
        );
    }
    assert_eq!(
        state.store.list().await.unwrap().len(),
        before,
        "a refused submit must not create a session"
    );
    assert!(!state
        .hub
        .handle(&other)
        .map(|h| h.is_busy())
        .unwrap_or(false));
}

#[tokio::test]
async fn scoped_interrupt_of_another_session_is_refused() {
    let state = test_state();
    let own = uuid::Uuid::new_v4();
    let other = uuid::Uuid::new_v4();
    state.hub.ensure_session(own, Some(own));
    state.hub.ensure_session(other, Some(other));
    let (mut ctx, mut rx) = scoped_ctx(own);

    handle_frame(
        &state,
        &mut ctx,
        true,
        Frame::request(
            3,
            payload(serde_json::json!({"type": "interrupt", "session_id": other})),
        ),
    )
    .await;
    assert_eq!(
        error_kind_of(&next_reply(&mut rx).await),
        Some(modron_protocol::ErrorKind::Forbidden)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn scoped_approve_cannot_resolve_another_sessions_request() {
    let state = test_state();
    let own = uuid::Uuid::new_v4();
    let other = uuid::Uuid::new_v4();
    state.hub.ensure_session(own, Some(own));
    let other_handle = state.hub.ensure_session(other, Some(other));

    // Park an approval on the OTHER session.
    let mut events = other_handle.subscribe();
    let call = ToolCall {
        id: "c1".into(),
        name: "bash".into(),
        arguments: serde_json::json!({"command": "echo hi"}),
    };
    let parked = Arc::clone(&other_handle);
    let mut gate = tokio::spawn(async move { parked.approve_gate(&call, None) });
    let request_id = loop {
        let live: LiveFrame = events.recv().await.unwrap();
        let frame: Frame = serde_json::from_str(&live.json).unwrap();
        if let Payload::ApprovalRequested { request_id, .. } = frame.payload {
            break request_id;
        }
    };

    // The tunnel for `own` tries to allow it.
    let (mut ctx, mut rx) = scoped_ctx(own);
    handle_frame(
        &state,
        &mut ctx,
        true,
        Frame::request(
            4,
            Payload::Approve {
                request_id: request_id.clone(),
                decision: Decision::AllowOnce,
            },
        ),
    )
    .await;
    let reply = next_reply(&mut rx).await;
    assert!(
        error_kind_of(&reply).is_some(),
        "foreign approval must be refused"
    );

    // Still parked: the gate has not been released by the refused frame.
    assert!(
        tokio::time::timeout(Duration::from_millis(200), &mut gate)
            .await
            .is_err(),
        "the other session's approval must still be pending"
    );
    // The owning session can still resolve it.
    other_handle
        .resolve(&request_id, Decision::AllowOnce)
        .expect("owner resolves its own request");
    assert!(gate.await.unwrap(), "owner's allow releases the gate");
}

#[tokio::test]
async fn scoped_ping_keeps_the_tunnel_open() {
    let state = test_state();
    let own = uuid::Uuid::new_v4();
    state.hub.ensure_session(own, Some(own));
    let (mut ctx, mut rx) = scoped_ctx(own);

    // The tunnel's periodic synthetic ping must not hit "send hello first".
    let flow = handle_frame(&state, &mut ctx, true, Frame::request(0, Payload::Ping {})).await;
    assert!(
        flow == Flow::Continue,
        "a ping on a fresh tunnel must not close it"
    );
    assert!(matches!(
        next_reply(&mut rx).await.payload,
        Payload::Pong {}
    ));
}

// ---- Portal host registration and redial (host token on the tunnel, session directory) ----

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Instant;

use futures::{SinkExt as _, StreamExt as _};
use modron_server::rc::{
    backoff_delay, connect_failure_text, redial_log_due, run_tunnel_timed, schedule_after,
    viewer_key, EnvLookup, ReconnectTiming, RegistrationTiming, HOST_KIND_ENV, HOST_TOKEN_ENV,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener as TokioListener, TcpStream};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::handshake::server::{
    ErrorResponse, Request as WsRequest, Response as WsResponse,
};
use tokio_tungstenite::tungstenite::http;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// One request the fake bridge's HTTP side received.
#[derive(Clone, Debug)]
struct Recorded {
    method: String,
    path: String,
    auth: Option<String>,
    body: String,
    at: Instant,
}

#[derive(Default)]
struct FakeShared {
    http: Mutex<Vec<Recorded>>,
    /// The `Authorization` header of each WebSocket upgrade (None = absent).
    ws_auth: Mutex<Vec<Option<String>>>,
    /// The request path of each WebSocket upgrade.
    ws_paths: Mutex<Vec<String>>,
    /// Statuses for successive PUTs; 204 once the queue is empty.
    put_statuses: Mutex<VecDeque<u16>>,
    /// When set, WebSocket upgrades are refused with this status.
    ws_refuse: Mutex<Option<u16>>,
    /// Text frames the agent sent over its socket, in order.
    agent_frames: Mutex<Vec<String>>,
    /// The live agent socket's outbound queue (test to agent), if one is up.
    to_agent: Mutex<Option<mpsc::UnboundedSender<String>>>,
}

/// A fake bridge on one loopback port: plain HTTP routes for registration,
/// and the `/agent/<key>` WebSocket. `drop_tunnel` closes every open agent
/// socket (the bridge side of a dropped connection); the port keeps
/// accepting, so the agent can redial.
struct FakeBridge {
    base: String,
    shared: Arc<FakeShared>,
    close: broadcast::Sender<()>,
}

impl FakeBridge {
    async fn start() -> Self {
        Self::start_on("127.0.0.1:0").await
    }

    async fn start_on(addr: &str) -> Self {
        let listener = TokioListener::bind(addr).await.unwrap();
        let base = format!("ws://{}", listener.local_addr().unwrap());
        let shared = Arc::new(FakeShared::default());
        let (close, _) = broadcast::channel(4);
        let (accept_shared, accept_close) = (Arc::clone(&shared), close.clone());
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                tokio::spawn(serve_conn(
                    sock,
                    Arc::clone(&accept_shared),
                    accept_close.subscribe(),
                ));
            }
        });
        Self {
            base,
            shared,
            close,
        }
    }

    fn requests(&self, method: &str) -> Vec<Recorded> {
        self.shared
            .http
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.method == method)
            .cloned()
            .collect()
    }

    fn puts(&self) -> Vec<Recorded> {
        self.requests("PUT")
    }

    fn deletes(&self) -> Vec<Recorded> {
        self.requests("DELETE")
    }

    fn ws_auth(&self) -> Vec<Option<String>> {
        self.shared.ws_auth.lock().unwrap().clone()
    }

    fn ws_paths(&self) -> Vec<String> {
        self.shared.ws_paths.lock().unwrap().clone()
    }

    /// How many snapshots the agent has sent over all its sockets.
    fn snapshots(&self) -> usize {
        self.shared
            .agent_frames
            .lock()
            .unwrap()
            .iter()
            .filter(|text| {
                serde_json::from_str::<Frame>(text)
                    .is_ok_and(|frame| matches!(frame.payload, Payload::Snapshot { .. }))
            })
            .count()
    }

    fn queue_put_statuses(&self, statuses: &[u16]) {
        self.shared
            .put_statuses
            .lock()
            .unwrap()
            .extend(statuses.iter().copied());
    }

    fn refuse_upgrades(&self, status: u16) {
        *self.shared.ws_refuse.lock().unwrap() = Some(status);
    }

    fn drop_tunnel(&self) {
        let _ = self.close.send(());
    }

    /// A viewer's hello, relayed the way the bridge relays it (role stamped).
    fn send_viewer_hello(&self) {
        let frame = Frame::request(
            9,
            Payload::Hello {
                client: "viewer".into(),
                protocol: modron_protocol::PROTOCOL_VERSION,
                token: None,
            },
        );
        let mut value = serde_json::to_value(&frame).unwrap();
        value["role"] = "viewer".into();
        let sender = self
            .shared
            .to_agent
            .lock()
            .unwrap()
            .clone()
            .expect("a live agent socket");
        sender.send(value.to_string()).unwrap();
    }
}

/// Route one connection: a WebSocket upgrade goes to the agent socket,
/// anything else is a plain HTTP request.
async fn serve_conn(sock: TcpStream, shared: Arc<FakeShared>, mut close: broadcast::Receiver<()>) {
    let head = peek_head(&sock).await;
    if head.to_ascii_lowercase().contains("upgrade: websocket") {
        let shared_for_cb = Arc::clone(&shared);
        // The Err type is fixed by tungstenite's handshake Callback.
        #[allow(clippy::result_large_err)]
        let callback =
            move |req: &WsRequest, resp: WsResponse| -> Result<WsResponse, ErrorResponse> {
                let auth = req
                    .headers()
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                shared_for_cb.ws_auth.lock().unwrap().push(auth);
                shared_for_cb
                    .ws_paths
                    .lock()
                    .unwrap()
                    .push(req.uri().path().to_string());
                let refuse = *shared_for_cb.ws_refuse.lock().unwrap();
                match refuse {
                    Some(status) => Err(http::Response::builder()
                        .status(status)
                        .body(Some("refused".to_string()))
                        .unwrap()),
                    None => Ok(resp),
                }
            };
        let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(sock, callback).await else {
            return;
        };
        let (to_agent_tx, mut to_agent_rx) = mpsc::unbounded_channel::<String>();
        *shared.to_agent.lock().unwrap() = Some(to_agent_tx);
        loop {
            tokio::select! {
                _ = close.recv() => {
                    let _ = ws.close(None).await;
                    break;
                }
                msg = ws.next() => match msg {
                    Some(Ok(WsMessage::Text(text))) => {
                        shared.agent_frames.lock().unwrap().push(text.to_string());
                    }
                    Some(Ok(_)) => {}
                    _ => break,
                },
                Some(text) = to_agent_rx.recv() => {
                    if ws.send(WsMessage::Text(text.into())).await.is_err() {
                        break;
                    }
                }
            }
        }
        *shared.to_agent.lock().unwrap() = None;
    } else {
        serve_http(sock, shared).await;
    }
}

/// The request head as far as it has arrived (peeked, not consumed).
async fn peek_head(sock: &TcpStream) -> String {
    let mut buf = vec![0u8; 4096];
    loop {
        let n = sock.peek(&mut buf).await.unwrap_or(0);
        let text = String::from_utf8_lossy(&buf[..n]).into_owned();
        if n == 0 || text.contains("\r\n\r\n") {
            return text;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

async fn serve_http(mut sock: TcpStream, shared: Arc<FakeShared>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    let head_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        let n = sock.read(&mut chunk).await.unwrap_or(0);
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.lines();
    let mut request_line = lines.next().unwrap_or_default().split_whitespace();
    let method = request_line.next().unwrap_or_default().to_string();
    let path = request_line.next().unwrap_or_default().to_string();
    let (mut auth, mut length) = (None, 0usize);
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let value = value.trim();
            if name.eq_ignore_ascii_case("authorization") {
                auth = Some(value.to_string());
            } else if name.eq_ignore_ascii_case("content-length") {
                length = value.parse().unwrap_or(0);
            }
        }
    }
    while buf.len() < head_end + length {
        let n = sock.read(&mut chunk).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = String::from_utf8_lossy(&buf[head_end..]).into_owned();
    let status = if method == "PUT" {
        shared
            .put_statuses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(204)
    } else {
        204
    };
    shared.http.lock().unwrap().push(Recorded {
        method,
        path,
        auth,
        body,
        at: Instant::now(),
    });
    let reason = match status {
        401 => "Unauthorized",
        403 => "Forbidden",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "No Content",
    };
    let reply =
        format!("HTTP/1.1 {status} {reason}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
    let _ = sock.write_all(reply.as_bytes()).await;
    let _ = sock.shutdown().await;
}

fn env_with(vars: &[(&str, &str)]) -> EnvLookup {
    let map: HashMap<String, String> = vars
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    Arc::new(move |name| map.get(name).cloned())
}

fn fast_timing() -> RegistrationTiming {
    RegistrationTiming {
        debounce: Duration::from_millis(100),
        heartbeat: Duration::from_millis(400),
        poll: Duration::from_millis(10),
    }
}

/// Redials in milliseconds: the injected clock for every reconnect test.
fn fast_reconnect() -> ReconnectTiming {
    ReconnectTiming {
        initial: Duration::from_millis(10),
        cap: Duration::from_millis(40),
        stable: Duration::from_millis(200),
        log_interval: Duration::from_secs(60),
    }
}

/// Poll `ok` until it holds, failing the test after 5 seconds.
async fn eventually(what: &str, mut ok: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !ok() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

/// Stop a tunnel the way `start_remote_control`'s replacement and
/// `forget_session_state` do: abort the task.
async fn stop(task: JoinHandle<anyhow::Result<()>>) {
    task.abort();
    let _ = task.await;
}

/// Open a tunnel for a fresh session on `base`, with its share key minted
/// (as `start_remote_control` does). Returns the state, the session, the
/// share key and the tunnel task.
async fn open_tunnel_to(
    base: String,
    lookup: EnvLookup,
    timing: RegistrationTiming,
    reconnect: ReconnectTiming,
    title: &str,
) -> (
    Arc<ServerState>,
    SessionId,
    String,
    JoinHandle<anyhow::Result<()>>,
) {
    let state = test_state();
    let session = state.store.create_session(title).await.unwrap().id;
    state.hub.ensure_session(session, Some(session));
    let key = modron_server::rc::share_key();
    state.rc_keys.lock().unwrap().insert(session, key.clone());
    let task = tokio::spawn(run_tunnel_timed(
        lookup,
        timing,
        reconnect,
        Arc::clone(&state),
        session,
        base,
        key.clone(),
    ));
    (state, session, key, task)
}

async fn open_tunnel(
    bridge: &FakeBridge,
    lookup: EnvLookup,
    timing: RegistrationTiming,
    title: &str,
) -> (
    Arc<ServerState>,
    SessionId,
    String,
    JoinHandle<anyhow::Result<()>>,
) {
    open_tunnel_to(bridge.base.clone(), lookup, timing, fast_reconnect(), title).await
}

#[tokio::test]
async fn host_token_is_sent_on_the_tunnel_handshake_only_when_set() {
    let with_token = FakeBridge::start().await;
    let (_state, _session, _key, task) = open_tunnel(
        &with_token,
        env_with(&[(HOST_TOKEN_ENV, "tok-123")]),
        fast_timing(),
        "with token",
    )
    .await;
    eventually("the tunnel upgrade", || !with_token.ws_auth().is_empty()).await;
    assert_eq!(
        with_token.ws_auth(),
        vec![Some("Bearer tok-123".to_string())]
    );
    stop(task).await;

    let without = FakeBridge::start().await;
    let (_state, _session, _key, task) =
        open_tunnel(&without, env_with(&[]), fast_timing(), "no token").await;
    eventually("the tunnel upgrade", || !without.ws_auth().is_empty()).await;
    assert_eq!(without.ws_auth(), vec![None], "no header without a token");
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(without.puts().is_empty(), "no registration without a token");
    stop(task).await;
}

#[tokio::test]
async fn registration_put_carries_exactly_the_five_fields() {
    let bridge = FakeBridge::start().await;
    let lookup = env_with(&[
        (HOST_TOKEN_ENV, "tok-abc"),
        (HOST_KIND_ENV, "cloud"),
        ("HOSTNAME", "box-7"),
    ]);
    let (_state, session, key, task) =
        open_tunnel(&bridge, lookup, fast_timing(), "Fix the parser").await;
    eventually("the first PUT", || !bridge.puts().is_empty()).await;

    let put = bridge.puts().remove(0);
    assert_eq!(put.path, format!("/api/host/sessions/{session}"));
    assert_eq!(put.auth.as_deref(), Some("Bearer tok-abc"));
    let body: serde_json::Value = serde_json::from_str(&put.body).unwrap();
    let object = body.as_object().unwrap();
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["host_kind", "host_label", "state", "title", "viewer_key"],
        "exactly five fields"
    );
    assert_eq!(body["title"], "Fix the parser");
    assert_eq!(body["host_label"], "box-7");
    assert_eq!(body["host_kind"], "cloud");
    assert_eq!(body["viewer_key"], viewer_key(&key).as_str());
    assert_eq!(body["state"], "idle");

    stop(task).await;
}

#[tokio::test]
async fn a_session_without_a_title_is_named_by_its_short_id() {
    let bridge = FakeBridge::start().await;
    let lookup = env_with(&[(HOST_TOKEN_ENV, "tok")]);
    let (_state, session, _key, task) = open_tunnel(&bridge, lookup, fast_timing(), "").await;
    eventually("the first PUT", || !bridge.puts().is_empty()).await;
    let body: serde_json::Value = serde_json::from_str(&bridge.puts()[0].body).unwrap();
    let short = &session.simple().to_string()[..8];
    assert_eq!(body["title"], format!("session {short}").as_str());
    stop(task).await;
}

#[tokio::test]
async fn running_state_change_is_pushed_and_puts_stay_debounced() {
    let bridge = FakeBridge::start().await;
    let timing = fast_timing();
    let lookup = env_with(&[(HOST_TOKEN_ENV, "tok")]);
    let (state, session, _key, task) = open_tunnel(&bridge, lookup, timing, "run").await;
    eventually("the first PUT", || !bridge.puts().is_empty()).await;

    state
        .hub
        .start_run(session, Box::pin(std::future::pending()))
        .expect("the run slot is free");
    eventually("a running PUT", || {
        bridge
            .puts()
            .iter()
            .any(|p| p.body.contains("\"state\":\"running\""))
    })
    .await;

    state.hub.handle(&session).unwrap().interrupt();
    eventually("an idle PUT after the run ends", || {
        bridge
            .puts()
            .last()
            .is_some_and(|p| p.body.contains("\"state\":\"idle\""))
    })
    .await;

    let puts = bridge.puts();
    for pair in puts.windows(2) {
        let gap = pair[1].at.duration_since(pair[0].at);
        assert!(
            gap >= timing.debounce,
            "PUTs {gap:?} apart, debounce is {:?}",
            timing.debounce
        );
    }

    stop(task).await;
}

#[tokio::test]
async fn heartbeat_repeats_the_put_while_the_tunnel_is_up() {
    let bridge = FakeBridge::start().await;
    let (_state, _session, _key, task) = open_tunnel(
        &bridge,
        env_with(&[(HOST_TOKEN_ENV, "tok")]),
        fast_timing(),
        "heartbeat",
    )
    .await;
    eventually("three PUTs", || bridge.puts().len() >= 3).await;
    stop(task).await;
}

#[tokio::test]
async fn a_dropped_socket_deletes_the_entry_before_the_redial_puts_it_again() {
    let bridge = FakeBridge::start().await;
    let (_state, session, _key, task) = open_tunnel(
        &bridge,
        env_with(&[(HOST_TOKEN_ENV, "tok-del")]),
        fast_timing(),
        "closing",
    )
    .await;
    eventually("the first PUT", || !bridge.puts().is_empty()).await;
    bridge.drop_tunnel();
    eventually("the DELETE of the dropped connection", || {
        bridge.deletes().len() == 1
    })
    .await;

    let deletes = bridge.deletes();
    assert_eq!(deletes[0].path, format!("/api/host/sessions/{session}"));
    assert_eq!(deletes[0].auth.as_deref(), Some("Bearer tok-del"));
    eventually("the redial's PUT", || bridge.puts().len() >= 2).await;
    stop(task).await;
}

#[tokio::test]
async fn portal_not_configured_turns_registration_off_for_the_tunnel() {
    let bridge = FakeBridge::start().await;
    bridge.queue_put_statuses(&[503]);
    let timing = RegistrationTiming {
        debounce: Duration::from_millis(10),
        heartbeat: Duration::from_millis(50),
        poll: Duration::from_millis(5),
    };
    let (_state, _session, _key, task) =
        open_tunnel(&bridge, env_with(&[(HOST_TOKEN_ENV, "tok")]), timing, "503").await;
    eventually("the 503 PUT", || bridge.puts().len() == 1).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(bridge.puts().len(), 1, "no retries after 503");
    assert!(!task.is_finished(), "the tunnel itself stays up");
    stop(task).await;
    assert!(bridge.deletes().is_empty(), "nothing to delete after 503");
}

#[tokio::test]
async fn refused_host_token_stops_registration_with_no_further_requests() {
    let bridge = FakeBridge::start().await;
    bridge.queue_put_statuses(&[401]);
    let timing = RegistrationTiming {
        debounce: Duration::from_millis(10),
        heartbeat: Duration::from_millis(50),
        poll: Duration::from_millis(5),
    };
    let (_state, _session, _key, task) = open_tunnel(
        &bridge,
        env_with(&[(HOST_TOKEN_ENV, "tok-bad")]),
        timing,
        "401",
    )
    .await;
    eventually("the 401 PUT", || bridge.puts().len() == 1).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(bridge.puts().len(), 1, "no retries after 401");
    stop(task).await;
    assert!(
        bridge.deletes().is_empty(),
        "no DELETE after a refused token"
    );
    assert_eq!(bridge.ws_auth(), vec![Some("Bearer tok-bad".to_string())]);
}

#[tokio::test]
async fn other_failures_are_retried_on_the_next_heartbeat() {
    let bridge = FakeBridge::start().await;
    bridge.queue_put_statuses(&[500]);
    let (_state, _session, _key, task) = open_tunnel(
        &bridge,
        env_with(&[(HOST_TOKEN_ENV, "tok")]),
        fast_timing(),
        "retry",
    )
    .await;
    eventually("a retried PUT", || bridge.puts().len() >= 2).await;
    stop(task).await;
}

#[tokio::test]
async fn host_token_never_appears_in_tunnel_errors() {
    // A token that cannot be a header value fails before any connection.
    let bridge = FakeBridge::start().await;
    let (_state, _session, _key, task) = open_tunnel(
        &bridge,
        env_with(&[(HOST_TOKEN_ENV, "token-secret\nbad")]),
        fast_timing(),
        "bad header",
    )
    .await;
    let err = task.await.unwrap().unwrap_err();
    let text = format!("{err:#}");
    assert!(!text.contains("token-secret"), "token leaked: {text}");
    assert!(text.contains(HOST_TOKEN_ENV), "{text}");
    assert!(bridge.ws_auth().is_empty(), "no dial with a bad header");
}

#[tokio::test]
async fn unreachable_bridge_error_names_the_bridge_and_never_the_key_or_token() {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let bridge = format!("ws://127.0.0.1:{port}");
    let key = modron_server::rc::share_key();
    let err = tokio_tungstenite::connect_async(format!("{bridge}/agent/{key}"))
        .await
        .expect_err("nothing listens there");
    let text = connect_failure_text(&bridge, &key, Some("tok-secret-9"), &err);
    assert!(
        text.starts_with(&format!(
            "connecting to the remote-control bridge at {bridge}: "
        )),
        "{text}"
    );
    assert!(
        text.contains("refused"),
        "the transport cause is shown: {text}"
    );
    assert!(!text.contains(&key), "the share key never appears: {text}");
    assert!(
        !text.contains("tok-secret-9"),
        "the token never appears: {text}"
    );
}

#[tokio::test]
async fn bridge_down_at_start_is_redialled_until_it_comes_up_and_registers() {
    // Reserve a port, then leave it with nothing listening.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let (_state, session, key, task) = open_tunnel_to(
        format!("ws://127.0.0.1:{port}"),
        env_with(&[(HOST_TOKEN_ENV, "tok")]),
        fast_timing(),
        fast_reconnect(),
        "down at start",
    )
    .await;
    // Several refused dials go by, each after one injected backoff step.
    tokio::time::sleep(fast_reconnect().cap * 4).await;
    assert!(
        !task.is_finished(),
        "a refused dial is retried, not given up"
    );

    let bridge = FakeBridge::start_on(&format!("127.0.0.1:{port}")).await;
    eventually("the tunnel connects", || !bridge.ws_auth().is_empty()).await;
    eventually("the registration", || !bridge.puts().is_empty()).await;
    assert_eq!(bridge.ws_paths(), vec![format!("/agent/{key}")]);
    assert_eq!(
        bridge.puts()[0].path,
        format!("/api/host/sessions/{session}")
    );
    stop(task).await;
}

#[tokio::test]
async fn dropped_socket_redials_with_the_same_key_and_a_late_viewer_gets_a_snapshot() {
    let bridge = FakeBridge::start().await;
    let (_state, session, key, task) = open_tunnel(
        &bridge,
        env_with(&[(HOST_TOKEN_ENV, "tok")]),
        fast_timing(),
        "redial",
    )
    .await;
    eventually("the first connection", || bridge.ws_auth().len() == 1).await;
    eventually("the first snapshot", || bridge.snapshots() >= 1).await;

    bridge.drop_tunnel();
    eventually("the redial", || bridge.ws_auth().len() == 2).await;
    eventually("the reconnect's snapshot", || bridge.snapshots() >= 2).await;
    assert_eq!(
        bridge.ws_paths(),
        vec![format!("/agent/{key}"), format!("/agent/{key}")],
        "the redial reuses the share key"
    );
    assert!(!task.is_finished(), "the tunnel keeps running");

    // The re-registration carries the same directory entry.
    eventually("the redial's PUT", || {
        bridge
            .puts()
            .last()
            .is_some_and(|p| p.path == format!("/api/host/sessions/{session}"))
    })
    .await;
    let viewer = viewer_key(&key);
    for put in bridge.puts() {
        let body: serde_json::Value = serde_json::from_str(&put.body).unwrap();
        assert_eq!(body["viewer_key"], viewer.as_str());
    }

    // A viewer that attaches after the reconnect gets a fresh snapshot.
    let before = bridge.snapshots();
    bridge.send_viewer_hello();
    eventually("the late viewer's snapshot", || bridge.snapshots() > before).await;
    stop(task).await;
}

#[tokio::test]
async fn a_refused_host_token_ends_the_tunnel_and_is_never_redialled() {
    let bridge = FakeBridge::start().await;
    bridge.refuse_upgrades(401);
    let (_state, _session, _key, task) = open_tunnel(
        &bridge,
        env_with(&[(HOST_TOKEN_ENV, "tok-bad")]),
        fast_timing(),
        "401 upgrade",
    )
    .await;
    let outcome = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("the tunnel gives up on a refusal")
        .expect("the task does not panic");
    let text = format!("{:#}", outcome.unwrap_err());
    assert!(text.contains(HOST_TOKEN_ENV), "the fix is named: {text}");
    assert!(!text.contains("tok-bad"), "the token never appears: {text}");

    tokio::time::sleep(fast_reconnect().cap * 5).await;
    assert_eq!(bridge.ws_auth().len(), 1, "no redial after a refused token");
}

#[tokio::test]
async fn a_stopped_tunnel_is_never_redialled() {
    let bridge = FakeBridge::start().await;
    let (_state, _session, _key, task) = open_tunnel(
        &bridge,
        env_with(&[(HOST_TOKEN_ENV, "tok")]),
        fast_timing(),
        "stop",
    )
    .await;
    eventually("the first connection", || bridge.ws_auth().len() == 1).await;
    stop(task).await;
    bridge.drop_tunnel();
    tokio::time::sleep(fast_reconnect().cap * 5).await;
    assert_eq!(bridge.ws_auth().len(), 1, "no redial after a stop");
}

#[tokio::test]
async fn a_forgotten_session_key_ends_the_tunnel_without_a_redial() {
    let bridge = FakeBridge::start().await;
    let (state, session, _key, task) = open_tunnel(
        &bridge,
        env_with(&[(HOST_TOKEN_ENV, "tok")]),
        fast_timing(),
        "deleted",
    )
    .await;
    eventually("the first connection", || bridge.ws_auth().len() == 1).await;
    // What deleting the session does to its server-side state (see
    // `forget_session_state`), minus the abort the supervisor does not need.
    state.rc_keys.lock().unwrap().remove(&session);
    bridge.drop_tunnel();
    let outcome = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("the tunnel ends once its key is gone")
        .expect("the task does not panic");
    outcome.expect("a stopped tunnel is not an error");
    tokio::time::sleep(fast_reconnect().cap * 5).await;
    assert_eq!(bridge.ws_auth().len(), 1, "no redial for a deleted session");
}

/// `modron serve` exit: the tunnel is aborted after `begin_exit`, the path
/// `run_serve` takes. The entry must stay (the portal lists the session
/// offline); a DELETE raced the exit and removed the row in some runs.
#[tokio::test]
async fn an_exiting_serve_keeps_its_portal_entry_when_its_tunnel_is_aborted() {
    let bridge = FakeBridge::start().await;
    let timing = RegistrationTiming {
        debounce: Duration::from_millis(10),
        heartbeat: Duration::from_millis(50),
        poll: Duration::from_millis(5),
    };
    let (state, _session, _key, task) = open_tunnel(
        &bridge,
        env_with(&[(HOST_TOKEN_ENV, "tok")]),
        timing,
        "exit",
    )
    .await;
    eventually("the first PUT", || bridge.puts().len() == 1).await;
    state
        .portal_exiting
        .store(true, std::sync::atomic::Ordering::SeqCst);
    stop(task).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        bridge.deletes().is_empty(),
        "an exiting serve must not DELETE its entry"
    );
}

/// The control for the test above: the same abort without an exit still
/// removes the entry (a session forgotten while the serve runs).
#[tokio::test]
async fn an_aborted_tunnel_outside_an_exit_still_deletes_its_entry() {
    let bridge = FakeBridge::start().await;
    let timing = RegistrationTiming {
        debounce: Duration::from_millis(10),
        heartbeat: Duration::from_millis(50),
        poll: Duration::from_millis(5),
    };
    let (_state, session, _key, task) = open_tunnel(
        &bridge,
        env_with(&[(HOST_TOKEN_ENV, "tok")]),
        timing,
        "forget",
    )
    .await;
    eventually("the first PUT", || bridge.puts().len() == 1).await;
    stop(task).await;
    eventually("the DELETE", || bridge.deletes().len() == 1).await;
    assert_eq!(
        bridge.deletes()[0].path,
        format!("/api/host/sessions/{session}")
    );
}

#[tokio::test]
async fn server_shutdown_stops_the_tunnel_for_good() {
    let bridge = FakeBridge::start().await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".git")).unwrap();
    let options = modron_server::ServeOptions {
        remote_control: Some(bridge.base.clone()),
        ..serve_options(dir.path())
    };
    let server = modron_server::bind_server(serve_engine(dir.path()), &options)
        .await
        .unwrap();
    eventually("the tunnel's websocket", || bridge.ws_auth().len() == 1).await;

    server.shutdown();
    bridge.drop_tunnel();
    tokio::time::sleep(fast_reconnect().cap * 5).await;
    assert_eq!(bridge.ws_auth().len(), 1, "no redial after shutdown");
}

#[test]
fn backoff_doubles_to_the_cap_and_jitter_stays_within_half_to_full() {
    let timing = ReconnectTiming::default();
    let full: Vec<u64> = (0..8)
        .map(|n| backoff_delay(&timing, n, 1.0).as_secs())
        .collect();
    assert_eq!(full, [1, 2, 4, 8, 16, 32, 60, 60]);
    let half: Vec<u128> = (0..8)
        .map(|n| backoff_delay(&timing, n, 0.0).as_millis())
        .collect();
    assert_eq!(half, [500, 1000, 2000, 4000, 8000, 16000, 30000, 30000]);
    assert_eq!(backoff_delay(&timing, 500, 1.0), Duration::from_secs(60));
    assert_eq!(backoff_delay(&timing, 3, 0.5), Duration::from_secs(6));
    assert_eq!(
        backoff_delay(&timing, 0, f64::NAN),
        Duration::from_millis(500)
    );
}

#[test]
fn a_connection_up_for_the_stable_window_resets_the_schedule() {
    let timing = ReconnectTiming::default();
    let mut failures = 0u32;
    let mut delays = Vec::new();
    for _ in 0..4 {
        delays.push(backoff_delay(&timing, failures, 1.0).as_secs());
        failures += 1;
    }
    // A dial that stayed up 30 s starts the schedule over.
    failures = schedule_after(failures, Duration::from_secs(30), &timing);
    assert_eq!(failures, 0);
    delays.push(backoff_delay(&timing, failures, 1.0).as_secs());
    assert_eq!(delays, [1, 2, 4, 8, 1]);
    assert_eq!(schedule_after(7, Duration::from_secs(29), &timing), 7);
}

#[test]
fn redials_at_the_cap_log_at_most_once_a_minute() {
    let timing = ReconnectTiming::default();
    assert!(redial_log_due(
        Duration::from_secs(4),
        &timing,
        Some(Duration::from_millis(1))
    ));
    assert!(redial_log_due(Duration::from_secs(60), &timing, None));
    assert!(!redial_log_due(
        Duration::from_secs(60),
        &timing,
        Some(Duration::from_secs(59))
    ));
    assert!(redial_log_due(
        Duration::from_secs(60),
        &timing,
        Some(Duration::from_secs(60))
    ));
}

/// An engine over an in-memory store whose working directory is `dir`.
fn serve_engine(dir: &std::path::Path) -> Engine {
    let store: Arc<dyn SessionStore> = Arc::new(modron_core::testing::InMemoryStore::new());
    Engine {
        selector: Arc::new(modron_core::testing::StaticSelector::new(
            "mock-model",
            "mock",
        )),
        providers: Default::default(),
        tools: Arc::new(modron_core::ToolRegistry::new()),
        store,
        config: EngineConfig {
            default_model: "mock-model".into(),
            default_provider: "mock".into(),
            working_dir: dir.to_path_buf(),
            ..EngineConfig::default()
        },
    }
}

/// `modron serve` options that touch nothing outside `dir`: WebSocket off,
/// the Unix socket and the clients directory under `dir`.
fn serve_options(dir: &std::path::Path) -> modron_server::ServeOptions {
    modron_server::ServeOptions {
        listen: Some("disabled".into()),
        socket: Some(dir.join("serve.sock")),
        clients_dir: Some(dir.join("clients")),
        ..modron_server::ServeOptions::default()
    }
}

#[tokio::test]
async fn serve_flag_opens_the_tunnel_at_start_and_writes_the_share_file() {
    let bridge = FakeBridge::start().await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".git")).unwrap();
    let share_file = dir.path().join("share-url");
    let options = modron_server::ServeOptions {
        remote_control: Some(bridge.base.clone()),
        share_url_file: Some(share_file.clone()),
        ..serve_options(dir.path())
    };
    let server = modron_server::bind_server(serve_engine(dir.path()), &options)
        .await
        .unwrap();

    // No client attached: the tunnel dials the bridge on its own.
    eventually("the tunnel's websocket", || bridge.ws_auth().len() == 1).await;
    let agent_key = server
        .state
        .rc_keys
        .lock()
        .unwrap()
        .values()
        .next()
        .cloned()
        .expect("one session has a share key");
    assert_eq!(server.state.rc_tunnels.lock().unwrap().len(), 1);
    // The git repository got its project marker.
    assert!(dir.path().join(".modron").is_dir());

    // The share file holds the viewer page for that tunnel, never the agent key.
    let http = bridge.base.replacen("ws://", "http://", 1);
    let shared = std::fs::read_to_string(&share_file).unwrap();
    assert_eq!(
        shared.trim(),
        format!("{http}/s/{}", viewer_key(&agent_key))
    );
    assert!(!shared.contains(&agent_key));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&share_file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    for tunnel in server.state.rc_tunnels.lock().unwrap().values() {
        tunnel.abort();
    }
    server.shutdown();
    bridge.drop_tunnel();
}

#[tokio::test]
async fn serve_without_the_flag_dials_nothing() {
    let bridge = FakeBridge::start().await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".git")).unwrap();
    let server = modron_server::bind_server(serve_engine(dir.path()), &serve_options(dir.path()))
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(bridge.ws_auth().is_empty());
    assert!(server.state.rc_keys.lock().unwrap().is_empty());
    assert!(server.state.rc_tunnels.lock().unwrap().is_empty());
    assert!(!dir.path().join(".modron").exists());
    server.shutdown();
}

#[tokio::test]
async fn serve_flag_with_an_invalid_bridge_fails_before_binding() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".git")).unwrap();
    // https is publish's scheme; the tunnel takes ws:// or wss://.
    let options = modron_server::ServeOptions {
        remote_control: Some("https://bridge.example".into()),
        ..serve_options(dir.path())
    };
    let err = modron_server::bind_server(serve_engine(dir.path()), &options)
        .await
        .err()
        .expect("an https bridge is refused");
    assert!(err.to_string().contains("ws://"), "{err}");
    assert!(!dir.path().join("serve.sock").exists());
    assert!(!dir.path().join(".modron").exists());
}

#[tokio::test]
async fn serve_flag_refuses_a_directory_that_is_not_a_project() {
    let dir = tempfile::tempdir().unwrap();
    let options = modron_server::ServeOptions {
        remote_control: Some("ws://127.0.0.1:9".into()),
        ..serve_options(dir.path())
    };
    let err = modron_server::bind_server(serve_engine(dir.path()), &options)
        .await
        .err()
        .expect("a plain directory is refused");
    assert!(err.to_string().contains("git repository"), "{err}");
    assert!(!dir.path().join("serve.sock").exists());
}

#[tokio::test]
async fn share_url_file_needs_the_remote_control_flag() {
    let dir = tempfile::tempdir().unwrap();
    let options = modron_server::ServeOptions {
        share_url_file: Some(dir.path().join("share-url")),
        ..serve_options(dir.path())
    };
    let err = modron_server::bind_server(serve_engine(dir.path()), &options)
        .await
        .err()
        .expect("a share file without a tunnel is refused");
    assert!(err.to_string().contains("--remote-control"), "{err}");
}
