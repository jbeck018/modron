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
    let mut gate = tokio::spawn(async move { parked.approve_gate(&call) });
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
