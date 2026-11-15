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

// ---- Portal host registration (host token on the tunnel, session directory) ----

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Instant;

use futures::StreamExt as _;
use modron_server::rc::{
    run_tunnel_with, viewer_key, EnvLookup, RegistrationTiming, HOST_KIND_ENV, HOST_TOKEN_ENV,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener as TokioListener, TcpStream};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::handshake::server::{
    Request as WsRequest, Response as WsResponse,
};

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
    /// Statuses for successive PUTs; 204 once the queue is empty.
    put_statuses: Mutex<VecDeque<u16>>,
}

/// A fake bridge on one loopback port: plain HTTP routes for registration,
/// and the `/agent/<key>` WebSocket, which stays open until `close_tunnel`.
struct FakeBridge {
    base: String,
    shared: Arc<FakeShared>,
    close: Arc<Notify>,
}

impl FakeBridge {
    async fn start() -> Self {
        let listener = TokioListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("ws://{}", listener.local_addr().unwrap());
        let shared = Arc::new(FakeShared::default());
        let close = Arc::new(Notify::new());
        let (shared_for_accept, close_for_accept) = (Arc::clone(&shared), Arc::clone(&close));
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let (shared, close) = (
                    Arc::clone(&shared_for_accept),
                    Arc::clone(&close_for_accept),
                );
                tokio::spawn(serve_conn(sock, shared, close));
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

    fn queue_put_statuses(&self, statuses: &[u16]) {
        self.shared
            .put_statuses
            .lock()
            .unwrap()
            .extend(statuses.iter().copied());
    }

    fn close_tunnel(&self) {
        self.close.notify_one();
    }
}

/// Route one connection: a WebSocket upgrade goes to the agent socket,
/// anything else is a plain HTTP request.
async fn serve_conn(sock: TcpStream, shared: Arc<FakeShared>, close: Arc<Notify>) {
    let head = peek_head(&sock).await;
    if head.to_ascii_lowercase().contains("upgrade: websocket") {
        let shared_for_cb = Arc::clone(&shared);
        // The Err type is fixed by tungstenite's handshake Callback.
        #[allow(clippy::result_large_err)]
        let callback = move |req: &WsRequest, resp: WsResponse| {
            let auth = req
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            shared_for_cb.ws_auth.lock().unwrap().push(auth);
            Ok(resp)
        };
        let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(sock, callback).await else {
            return;
        };
        loop {
            tokio::select! {
                _ = close.notified() => {
                    let _ = ws.close(None).await;
                    break;
                }
                msg = ws.next() => {
                    if !matches!(msg, Some(Ok(_))) {
                        break;
                    }
                }
            }
        }
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

/// Open a tunnel for a fresh session on `bridge`. Returns the state, the
/// session, the share key and the tunnel task.
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
    let state = test_state();
    let session = state.store.create_session(title).await.unwrap().id;
    state.hub.ensure_session(session, Some(session));
    let key = modron_server::rc::share_key();
    let task = tokio::spawn(run_tunnel_with(
        lookup,
        timing,
        Arc::clone(&state),
        session,
        bridge.base.clone(),
        key.clone(),
    ));
    (state, session, key, task)
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
    with_token.close_tunnel();
    task.await.unwrap().unwrap();

    let without = FakeBridge::start().await;
    let (_state, _session, _key, task) =
        open_tunnel(&without, env_with(&[]), fast_timing(), "no token").await;
    eventually("the tunnel upgrade", || !without.ws_auth().is_empty()).await;
    assert_eq!(without.ws_auth(), vec![None], "no header without a token");
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(without.puts().is_empty(), "no registration without a token");
    without.close_tunnel();
    task.await.unwrap().unwrap();
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

    bridge.close_tunnel();
    task.await.unwrap().unwrap();
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
    bridge.close_tunnel();
    task.await.unwrap().unwrap();
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

    bridge.close_tunnel();
    task.await.unwrap().unwrap();
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
    bridge.close_tunnel();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn closing_the_tunnel_deletes_the_entry() {
    let bridge = FakeBridge::start().await;
    let (_state, session, _key, task) = open_tunnel(
        &bridge,
        env_with(&[(HOST_TOKEN_ENV, "tok-del")]),
        fast_timing(),
        "closing",
    )
    .await;
    eventually("the first PUT", || !bridge.puts().is_empty()).await;
    bridge.close_tunnel();
    task.await.unwrap().unwrap();

    let deletes = bridge.deletes();
    assert_eq!(deletes.len(), 1, "one DELETE on close");
    assert_eq!(deletes[0].path, format!("/api/host/sessions/{session}"));
    assert_eq!(deletes[0].auth.as_deref(), Some("Bearer tok-del"));
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
    bridge.close_tunnel();
    task.await.unwrap().unwrap();
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
    bridge.close_tunnel();
    task.await.unwrap().unwrap();
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
    bridge.close_tunnel();
    task.await.unwrap().unwrap();
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

    // A refused connection with a valid token: the error names the bridge only.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead = format!("ws://{}", listener.local_addr().unwrap());
    drop(listener);
    let state = test_state();
    let session = state.store.create_session("dead").await.unwrap().id;
    state.hub.ensure_session(session, Some(session));
    let err = run_tunnel_with(
        env_with(&[(HOST_TOKEN_ENV, "token-secret-2")]),
        fast_timing(),
        state,
        session,
        dead,
        modron_server::rc::share_key(),
    )
    .await
    .unwrap_err();
    assert!(
        !format!("{err:#}").contains("token-secret-2"),
        "token leaked: {err:#}"
    );
}
