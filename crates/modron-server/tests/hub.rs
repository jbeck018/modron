//! Hub unit tests: seq monotonicity, the LiveState projection, and the
//! approval gate round-trip (park -> resolve -> sticky allow-rest).

use std::sync::Arc;
use std::time::Duration;

use modron_core::{
    Engine, EngineConfig, SessionEvent, SessionId, SessionStore, StreamDelta, ToolActivity,
    ToolCall, Usage,
};
use modron_protocol::{Decision, Frame, Payload, WireDelta};
use modron_server::{BroadcastingStore, SessionHub};
use tokio::sync::broadcast;

fn test_hub() -> (SessionHub, SessionId) {
    let hub = SessionHub::empty();
    let store: Arc<dyn SessionStore> = Arc::new(modron_core::testing::InMemoryStore::new());
    let engine = Engine {
        selector: Arc::new(modron_core::testing::StaticSelector::new(
            "mock-model",
            "mock",
        )),
        providers: Default::default(),
        tools: Arc::new(modron_core::ToolRegistry::new()),
        store: store.clone(),
        config: EngineConfig {
            default_model: "mock-model".into(),
            default_provider: "mock".into(),
            ..EngineConfig::default()
        },
    };
    hub.set_engine(engine);
    let id = uuid::Uuid::new_v4();
    hub.ensure_session(id, None);
    (hub, id)
}

/// Parse a fanned-out LiveFrame back into a Frame (the broadcast carries
/// pre-serialized JSON since the 2026-11-12 perf pass).
fn parse_live(live: modron_server::hub::LiveFrame) -> Frame {
    serde_json::from_str(&live.json).expect("live frame json parses")
}

/// Seq rises strictly with every published frame, session-scoped.
#[tokio::test]
async fn seq_is_monotonic_per_session() {
    let (hub, id) = test_hub();
    let handle = hub.handle(&id).unwrap();
    let mut rx = handle.subscribe();

    for i in 0..5 {
        handle.publish(Payload::Note {
            text: format!("n{i}"),
        });
    }
    let mut last = 0;
    for _ in 0..5 {
        let frame = parse_live(rx.recv().await.unwrap());
        assert_eq!(frame.session, Some(id));
        assert!(
            frame.seq > last,
            "seq must strictly rise: {} after {last}",
            frame.seq
        );
        last = frame.seq;
    }
    // The watermark matches what was published.
    assert_eq!(handle.current_seq(), 5);
}

/// Separate sessions count independently.
#[tokio::test]
async fn seq_counters_are_per_session() {
    let (hub, id) = test_hub();
    let other = uuid::Uuid::new_v4();
    hub.ensure_session(other, None);
    let a = hub.handle(&id).unwrap();
    let b = hub.handle(&other).unwrap();
    a.publish(Payload::Note { text: "a1".into() });
    a.publish(Payload::Note { text: "a2".into() });
    b.publish(Payload::Note { text: "b1".into() });
    assert_eq!(a.current_seq(), 2);
    assert_eq!(b.current_seq(), 1);
}

/// The LiveState projection: streaming buffers, grouped tool view, usage
/// fold, and the labels.
#[tokio::test]
async fn live_state_projects_buffers_tracker_and_usage() {
    let (hub, id) = test_hub();
    let handle = hub.handle(&id).unwrap();

    handle.on_stream_delta(StreamDelta::Text("hel".into()));
    handle.on_stream_delta(StreamDelta::Reasoning("thinking".into()));
    handle.on_tool_activity(ToolActivity::Start {
        name: "bash".into(),
    });
    handle.on_tool_activity(ToolActivity::Done {
        name: "bash".into(),
        is_error: false,
        bytes: 120,
    });
    handle.on_persisted(&SessionEvent::Usage {
        model: "mock-model".into(),
        usage: Usage {
            input_tokens: 100,
            output_tokens: 10,
            cached_input_tokens: 40,
        },
    });

    let live = handle.live_state();
    assert_eq!(live.streaming_text, "hel");
    assert_eq!(live.streaming_reasoning, "thinking");
    assert_eq!(live.turn_tools.len(), 1);
    assert!(live.turn_tools[0].done);
    assert_eq!(live.turn_tools[0].bytes, Some(120));
    assert_eq!(live.cumulative_usage.input_tokens, 100);
    assert_eq!(live.cumulative_usage.output_tokens, 10);
    assert_eq!(live.model, "mock-model");
    assert_eq!(live.provider, "mock");
    assert!(!live.running);

    // Done clears the streaming buffers and the per-turn tool view.
    handle.on_stream_delta(StreamDelta::Done);
    let live = handle.live_state();
    assert_eq!(live.streaming_text, "");
    assert_eq!(live.streaming_reasoning, "");
    assert!(live.turn_tools.is_empty());
    // Usage persists across turns.
    assert_eq!(live.cumulative_usage.input_tokens, 100);
}

/// The approval gate: broadcast -> park -> resolve; Deny denies; AllowAll
/// approves AND arms the sticky allow-rest; double resolve errors.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn approval_gate_round_trip_and_sticky_allow_rest() {
    let (hub, id) = test_hub();
    let handle = hub.handle(&id).unwrap();
    let mut rx = handle.subscribe();
    let call = ToolCall {
        id: "c1".into(),
        name: "bash".into(),
        arguments: serde_json::json!({"command": "echo"}),
    };

    // Park the gate in a background task.
    let parked = Arc::clone(&handle);
    let call_clone = call.clone();
    let gate = tokio::spawn(async move { parked.approve_gate(&call_clone) });

    let frame = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("request frame")
        .map(parse_live)
        .unwrap();
    let request_id = match frame.payload {
        Payload::ApprovalRequested { request_id, call } => {
            assert_eq!(call.name, "bash");
            request_id
        }
        other => panic!("expected approval_requested, got {other:?}"),
    };
    // It is visible in the live projection while pending.
    let live = handle.live_state();
    assert!(live.pending_approval.is_some());

    handle.resolve(&request_id, Decision::Deny).unwrap();
    assert!(!tokio::time::timeout(Duration::from_secs(2), gate)
        .await
        .unwrap()
        .unwrap());
    assert!(handle.live_state().pending_approval.is_none());
    // Not sticky: a second call still asks (asserted by the re-prompt below).
    // Consume the Deny resolution echo before the next round.
    let resolved = parse_live(rx.recv().await.unwrap());
    assert!(matches!(
        resolved.payload,
        Payload::ApprovalResolved {
            decision: Decision::Deny,
            ..
        }
    ));

    // AllowAll: approves, arms allow-rest; the next call passes instantly.
    let parked = Arc::clone(&handle);
    let call_clone = call.clone();
    let gate = tokio::spawn(async move { parked.approve_gate(&call_clone) });
    let frame = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("second request frame")
        .map(parse_live)
        .unwrap();
    let request_id = match frame.payload {
        Payload::ApprovalRequested { request_id, .. } => request_id,
        other => panic!("expected approval_requested, got {other:?}"),
    };
    handle.resolve(&request_id, Decision::AllowAll).unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(2), gate)
        .await
        .unwrap()
        .unwrap());
    // Sticky: the next call passes instantly (the already-completed gate
    // above); the test below leans on it.

    // The AllowAll resolution echo is still in the stream; the sticky gate
    // then passes WITHOUT publishing anything more.
    let resolved = parse_live(rx.recv().await.unwrap());
    assert!(matches!(
        resolved.payload,
        Payload::ApprovalResolved {
            decision: Decision::AllowAll,
            ..
        }
    ));
    assert!(handle.approve_gate(&call));
    assert!(matches!(
        rx.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));

    // Unknown request ids never resolve.
    assert!(handle.resolve("req-ghost", Decision::Deny).is_err());

    // Allow-all is per RUN: starting the next run clears it, so the first
    // mutating call of that run asks again.
    hub.start_run(
        id,
        Box::pin(async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(modron_core::RunSummary {
                session_id: id,
                answer: "unreachable".into(),
                turns: 0,
                tool_calls: 0,
                usage: Default::default(),
                models_used: vec![],
            })
        }),
    )
    .unwrap();
    let parked = Arc::clone(&handle);
    let call_clone = call.clone();
    let gate = tokio::spawn(async move { parked.approve_gate(&call_clone) });
    let request_id = loop {
        let frame = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("the next run must ask again")
            .map(parse_live)
            .unwrap();
        if let Payload::ApprovalRequested { request_id, .. } = frame.payload {
            break request_id;
        }
    };
    handle.resolve(&request_id, Decision::Deny).unwrap();
    assert!(!tokio::time::timeout(Duration::from_secs(2), gate)
        .await
        .unwrap()
        .unwrap());
    handle.interrupt();
}

/// Interrupt: a run occupying the slot is aborted, a parked approver
/// releases (denied), TurnAborted is published exactly once, and the slot
/// frees.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupt_unblocks_a_parked_approver_and_aborts_the_run() {
    use modron_core::RunSummary;

    let (hub, id) = test_hub();
    let handle = hub.handle(&id).unwrap();
    let mut rx = handle.subscribe();
    let call = ToolCall {
        id: "c1".into(),
        name: "bash".into(),
        arguments: serde_json::json!({}),
    };

    // A long-running run occupies the slot (never completes on its own).
    hub.start_run(
        id,
        Box::pin(async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(RunSummary {
                session_id: id,
                answer: "unreachable".into(),
                turns: 0,
                tool_calls: 0,
                usage: Default::default(),
                models_used: vec![],
            })
        }),
    )
    .unwrap();

    // The engine parks in the gate mid-run.
    let parked = Arc::clone(&handle);
    let gate = tokio::spawn(async move { parked.approve_gate(&call) });
    let frame = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .map(parse_live)
        .unwrap();
    let _request_id = match frame.payload {
        Payload::ApprovalRequested { request_id, .. } => request_id,
        other => panic!("expected approval_requested, got {other:?}"),
    };

    assert!(handle.interrupt());
    // The parked approver returned (denied by the interrupt drain).
    assert!(!tokio::time::timeout(Duration::from_secs(2), gate)
        .await
        .unwrap()
        .unwrap());
    // The drained gate's Deny resolution lands BEFORE the abort (a client
    // must never render a phantom approval for an already-dead run - the
    // 2026-11-12 cycle-2 M3 fix), then TurnAborted exactly once.
    let resolved = parse_live(rx.recv().await.unwrap());
    assert!(matches!(
        resolved.payload,
        Payload::ApprovalResolved {
            decision: Decision::Deny,
            ..
        }
    ));
    let frame = parse_live(rx.recv().await.unwrap());
    assert!(matches!(frame.payload, Payload::TurnAborted {}));
    assert!(!handle.interrupt(), "second interrupt finds no active run");
    assert!(matches!(
        rx.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    assert!(!handle.is_busy(), "the run slot freed");
}

/// The wire delta conversion skips ToolCallReady (engine-internal).
#[tokio::test]
async fn tool_call_ready_deltas_never_publish() {
    let (hub, id) = test_hub();
    let handle = hub.handle(&id).unwrap();
    let mut rx = handle.subscribe();
    handle.on_stream_delta(StreamDelta::ToolCallReady(ToolCall {
        id: "c".into(),
        name: "read".into(),
        arguments: serde_json::json!({}),
    }));
    assert!(matches!(
        rx.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    // Text deltas publish with the wire shape.
    handle.on_stream_delta(StreamDelta::Text("hi".into()));
    let frame = parse_live(rx.recv().await.unwrap());
    assert!(matches!(
        frame.payload,
        Payload::Delta {
            delta: WireDelta::Text { .. }
        }
    ));
}

/// The broadcasting store persists, then fans out to the hub.
#[tokio::test]
async fn broadcasting_store_persists_then_fans_out() {
    let inner: Arc<dyn SessionStore> = Arc::new(modron_core::testing::InMemoryStore::new());
    let hub = SessionHub::empty();
    let engine = Engine {
        selector: Arc::new(modron_core::testing::StaticSelector::new("m", "mock")),
        providers: Default::default(),
        tools: Arc::new(modron_core::ToolRegistry::new()),
        store: inner.clone(),
        config: EngineConfig::default(),
    };
    hub.set_engine(engine);
    let store = BroadcastingStore::new(inner.clone(), hub.clone());

    // Unknown sessions must fail at the inner store, not fabricate one.
    let ghost = uuid::Uuid::new_v4();
    assert!(store
        .append(ghost, &SessionEvent::Task { text: "x".into() })
        .await
        .is_err());

    let meta = store.create_session("t").await.unwrap();
    hub.ensure_session(meta.id, None);
    let handle = hub.handle(&meta.id).unwrap();
    let mut rx = handle.subscribe();

    store
        .append(
            meta.id,
            &SessionEvent::Task {
                text: "hello".into(),
            },
        )
        .await
        .unwrap();

    // Persisted exactly once...
    let events = store.load(meta.id).await.unwrap();
    assert_eq!(events.len(), 1);
    // ...and broadcast to the session's subscribers.
    let frame = parse_live(rx.recv().await.unwrap());
    match frame.payload {
        Payload::Event { event } => match event {
            SessionEvent::Task { text } => assert_eq!(text, "hello"),
            other => panic!("expected task event, got {other:?}"),
        },
        other => panic!("expected event frame, got {other:?}"),
    }
}

/// The timeout path publishes ApprovalResolved{Deny} (cycle-2 M2 + cycle-3
/// regression pin): a client that rendered the request must see the
/// resolution, and pending must drain.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approval_timeout_publishes_a_deny_resolution() {
    let (hub, id) = test_hub();
    let handle = hub.handle(&id).unwrap();
    handle.set_approval_timeout(std::time::Duration::from_millis(100));
    let mut rx = handle.subscribe();
    let call = ToolCall {
        id: "c1".into(),
        name: "bash".into(),
        arguments: serde_json::json!({}),
    };
    let gate = {
        let parked = Arc::clone(&handle);
        let call = call.clone();
        tokio::spawn(async move { parked.approve_gate(&call) })
    };
    let frame = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("request frame")
        .map(parse_live)
        .unwrap();
    assert!(matches!(frame.payload, Payload::ApprovalRequested { .. }));

    let denied = tokio::time::timeout(Duration::from_secs(2), gate)
        .await
        .unwrap()
        .unwrap();
    assert!(!denied, "timeout denies");
    let resolved = parse_live(rx.recv().await.unwrap());
    assert!(
        matches!(
            resolved.payload,
            Payload::ApprovalResolved {
                decision: Decision::Deny,
                ..
            }
        ),
        "the client must see the timeout denial: {:?}",
        resolved.payload
    );
    assert!(handle.live_state().pending_approval.is_none());
}
