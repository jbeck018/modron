//! Hub unit tests: seq monotonicity, the LiveState projection, and the
//! approval gate round-trip (park -> resolve -> sticky allow-rest).

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{assert_approval_notify, next_live_frame};
use modron_core::permissions::{Rule, RuleEffect, RuleSet};
use modron_core::{
    ApprovalMode, ApprovalPolicy, Engine, EngineConfig, SandboxPolicy, SessionEvent, SessionId,
    SessionStore, StreamDelta, ToolActivity, ToolCall, Usage,
};
use modron_protocol::{
    ApprovalModeWire, ConfigPatch, Decision, Frame, Payload, RuleWire, WireDelta,
};
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

/// C5: the snapshot's mode, sandbox and allow/deny come from the
/// session's effective config, at creation and after SetConfig.
#[tokio::test]
async fn live_state_reflects_engine_config_and_config_changes() {
    let (hub, id) = test_hub();
    let handle = hub.handle(&id).unwrap();
    let live = handle.live_state();
    assert_eq!(live.approval_mode, Some(ApprovalModeWire::Ask));
    assert_eq!(live.sandbox, Some(SandboxPolicy::Off));
    assert!(live.allow.is_empty());
    assert!(live.deny.is_empty());

    handle
        .apply_config(&ConfigPatch {
            approval_mode: Some(ApprovalModeWire::ApproveAll),
            allow: Some(vec!["diagnostics".into()]),
            deny: Some(vec!["bash".into(), "write".into()]),
            ..ConfigPatch::default()
        })
        .unwrap();
    let live = handle.live_state();
    assert_eq!(live.approval_mode, Some(ApprovalModeWire::ApproveAll));
    assert_eq!(live.allow, vec!["diagnostics".to_string()]);
    assert_eq!(live.deny, vec!["bash".to_string(), "write".to_string()]);
}

/// C5: a session created from an engine already in yolo reports yolo on
/// its first snapshot, without any SetConfig.
#[tokio::test]
async fn new_session_reports_the_template_engine_mode() {
    let hub = SessionHub::empty();
    let store: Arc<dyn SessionStore> = Arc::new(modron_core::testing::InMemoryStore::new());
    let engine = Engine {
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
            approvals: ApprovalPolicy {
                mode: ApprovalMode::ApproveAll,
                deny: vec!["bash".into()],
                ..ApprovalPolicy::default()
            },
            ..EngineConfig::default()
        },
    };
    hub.set_engine(engine);
    let id = uuid::Uuid::new_v4();
    let handle = hub.ensure_session(id, None);
    let live = handle.live_state();
    assert_eq!(live.approval_mode, Some(ApprovalModeWire::ApproveAll));
    assert_eq!(live.deny, vec!["bash".to_string()]);
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
        call_id: None,
    });
    handle.on_tool_activity(ToolActivity::Done {
        name: "bash".into(),
        is_error: false,
        bytes: 120,
        call_id: None,
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
    let gate = tokio::spawn(async move { parked.approve_gate(&call_clone, None) });

    let frame = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("request frame")
        .map(parse_live)
        .unwrap();
    let request_id = match frame.payload {
        Payload::ApprovalRequested {
            request_id, call, ..
        } => {
            assert_eq!(call.name, "bash");
            request_id
        }
        other => panic!("expected approval_requested, got {other:?}"),
    };
    // The approval Notify follows the request, before the resolution.
    assert_approval_notify(&next_live_frame(&mut rx).await, "bash");
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
    let gate = tokio::spawn(async move { parked.approve_gate(&call_clone, None) });
    let frame = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("second request frame")
        .map(parse_live)
        .unwrap();
    let request_id = match frame.payload {
        Payload::ApprovalRequested { request_id, .. } => request_id,
        other => panic!("expected approval_requested, got {other:?}"),
    };
    assert_approval_notify(&next_live_frame(&mut rx).await, "bash");
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
    assert!(handle.approve_gate(&call, None));
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
    let gate = tokio::spawn(async move { parked.approve_gate(&call_clone, None) });
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

/// Each approval request publishes exactly one approval Notify, right after
/// the request, naming only the tool: the arguments never reach the text.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approval_request_publishes_one_notify_without_arguments() {
    let (hub, id) = test_hub();
    let handle = hub.handle(&id).unwrap();
    let mut rx = handle.subscribe();
    let call = ToolCall {
        id: "c1".into(),
        name: "bash".into(),
        arguments: serde_json::json!({"command": "rm -rf /secret/path"}),
    };
    let gate = {
        let parked = Arc::clone(&handle);
        let call = call.clone();
        tokio::spawn(async move { parked.approve_gate(&call, None) })
    };
    let request_id = match next_live_frame(&mut rx).await.payload {
        Payload::ApprovalRequested { request_id, .. } => request_id,
        other => panic!("expected approval_requested, got {other:?}"),
    };
    let notify = next_live_frame(&mut rx).await;
    assert_approval_notify(&notify, "bash");
    let raw = serde_json::to_string(&notify).unwrap();
    assert!(!raw.contains("secret"), "arguments leaked: {raw}");

    handle.resolve(&request_id, Decision::Deny).unwrap();
    // Exactly one Notify per request: the rest of the flow is the echo.
    let resolved = next_live_frame(&mut rx).await;
    assert!(matches!(resolved.payload, Payload::ApprovalResolved { .. }));
    assert!(matches!(
        rx.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    assert!(!tokio::time::timeout(Duration::from_secs(2), gate)
        .await
        .unwrap()
        .unwrap());
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
    let gate = tokio::spawn(async move { parked.approve_gate(&call, None) });
    let frame = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .map(parse_live)
        .unwrap();
    let _request_id = match frame.payload {
        Payload::ApprovalRequested { request_id, .. } => request_id,
        other => panic!("expected approval_requested, got {other:?}"),
    };
    assert_approval_notify(&next_live_frame(&mut rx).await, "bash");

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
        tokio::spawn(async move { parked.approve_gate(&call, None) })
    };
    let frame = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("request frame")
        .map(parse_live)
        .unwrap();
    assert!(matches!(frame.payload, Payload::ApprovalRequested { .. }));
    assert_approval_notify(&next_live_frame(&mut rx).await, "bash");

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

/// Template engine over `store`, shared by the LG regression tests.
fn engine_over(store: Arc<dyn SessionStore>) -> Engine {
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
            ..EngineConfig::default()
        },
    }
}

/// A run that never completes on its own (it occupies the run slot).
fn parked_run(id: SessionId) -> modron_server::RunFuture {
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
    })
}

/// A run the test completes on demand (`done` sends the success path).
fn completing_run(
    id: SessionId,
    done: tokio::sync::oneshot::Receiver<()>,
) -> modron_server::RunFuture {
    Box::pin(async move {
        let _ = done.await;
        Ok(modron_core::RunSummary {
            session_id: id,
            answer: "done".into(),
            turns: 0,
            tool_calls: 0,
            usage: Default::default(),
            models_used: vec![],
        })
    })
}

fn child_call(id: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "bash".into(),
        arguments: serde_json::json!({"command": "echo"}),
    }
}

/// H3b: a child's ask is published with its label, one grant covers the
/// run's other children (allow-all is the parent's sticky flag), and the
/// sticky pass publishes nothing new.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn child_asks_carry_the_agent_label_and_share_allow_all() {
    let (hub, id) = test_hub();
    let handle = hub.handle(&id).unwrap();
    let (_done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
    hub.start_run(id, completing_run(id, done_rx)).unwrap();
    let mut rx = handle.subscribe();

    let parked = Arc::clone(&handle);
    let first = child_call("c1");
    let gate = tokio::spawn(async move { parked.approve_gate(&first, Some("delegate")) });
    let frame = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("request frame")
        .map(parse_live)
        .unwrap();
    let request_id = match frame.payload {
        Payload::ApprovalRequested {
            request_id,
            call,
            agent,
        } => {
            assert_eq!(call.name, "bash");
            assert_eq!(
                agent.as_deref(),
                Some("delegate"),
                "the child label rides the ask"
            );
            request_id
        }
        other => panic!("expected approval_requested, got {other:?}"),
    };
    handle
        .resolve(&request_id, Decision::AllowAll)
        .expect("resolve the child's ask");
    assert!(gate.await.unwrap());

    // Another child of the same run: allowed without a new prompt.
    assert!(handle.approve_gate(&child_call("c2"), Some("persona")));
    loop {
        let live = parse_live(rx.recv().await.unwrap());
        if matches!(live.payload, Payload::ApprovalResolved { .. }) {
            break;
        }
    }
    assert!(matches!(
        rx.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
}

/// H3b: no approver outlives its run. A child asking after the run ended is
/// refused without a prompt; a child parked when the run ends is denied.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn child_asks_after_the_run_ends_are_refused() {
    let (hub, id) = test_hub();
    let handle = hub.handle(&id).unwrap();

    // Late ask: the run completed first.
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
    hub.start_run(id, completing_run(id, done_rx)).unwrap();
    done_tx.send(()).unwrap();
    for _ in 0..200 {
        if !handle.is_busy() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!handle.is_busy(), "the run ended");
    let mut rx = handle.subscribe();
    assert!(!handle.approve_gate(&child_call("late"), Some("delegate")));
    assert!(matches!(
        rx.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));

    // Parked ask: the run completes while the child waits for a decision.
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
    hub.start_run(id, completing_run(id, done_rx)).unwrap();
    let mut rx = handle.subscribe();
    let parked = Arc::clone(&handle);
    let gate =
        tokio::spawn(async move { parked.approve_gate(&child_call("parked"), Some("delegate")) });
    let _request = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("request frame");
    done_tx.send(()).unwrap();
    assert!(
        !gate.await.unwrap(),
        "a parked child ask is denied at run end"
    );
    assert!(handle.live_state().pending_approval.is_none());
}

/// LG-a: a rejected patch applies nothing - the status labels and the
/// engine config both still describe the session before the patch.
#[tokio::test]
async fn rejected_config_patch_applies_nothing() {
    let (hub, id) = test_hub();
    let handle = hub.handle(&id).unwrap();
    let before = handle.live_state();
    let result = handle.apply_config(&ConfigPatch {
        model: Some("other-model".into()),
        persona: Some("nope".into()),
        ..ConfigPatch::default()
    });
    assert!(result.is_err(), "unknown persona must reject the patch");
    let after = handle.live_state();
    assert_eq!(after.model, before.model, "model label must not move");
    assert_eq!(
        after.provider, before.provider,
        "provider label must not move"
    );
    assert_eq!(handle.engine_cell().config.default_model, "mock-model");
}

/// LG-e: a known session with no events keeps its id on every submit, even
/// after the adoption pin was consumed by an earlier (failed) first run.
#[tokio::test]
async fn adoption_pin_is_rearmed_for_a_later_submit_on_an_empty_session() {
    let store: Arc<dyn SessionStore> = Arc::new(modron_core::testing::InMemoryStore::new());
    let hub = SessionHub::empty();
    hub.set_engine(engine_over(Arc::clone(&store)));
    let meta = store.create_session("new").await.unwrap();
    let handle = hub.ensure_session(meta.id, Some(meta.id));

    // First run adopts the reserved session and consumes the pin.
    let first = handle.engine_cell().clone();
    assert_eq!(first.store.create_session("t").await.unwrap().id, meta.id);

    // A later submit on the still-empty session must land in that session.
    handle.rearm_adoption();
    let second = handle.engine_cell().clone();
    assert_eq!(second.store.create_session("t").await.unwrap().id, meta.id);
}

/// LG-c (sequential form): a run started right after an interrupt must
/// prompt for approval. The bad interleaving itself needs a seam; this
/// pins the observable contract the fix protects.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_started_after_an_interrupt_still_prompts_for_approval() {
    let (hub, id) = test_hub();
    let handle = hub.handle(&id).unwrap();
    let mut rx = handle.subscribe();
    hub.start_run(id, parked_run(id)).unwrap();
    assert!(handle.interrupt());

    hub.start_run(id, parked_run(id)).unwrap();
    let call = ToolCall {
        id: "c2".into(),
        name: "bash".into(),
        arguments: serde_json::json!({}),
    };
    let gate_handle = Arc::clone(&handle);
    let gate = tokio::spawn(async move { gate_handle.approve_gate(&call, None) });

    let request_id = loop {
        let frame = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("the new run's approval must be published")
            .map(parse_live)
            .unwrap();
        if let Payload::ApprovalRequested { request_id, .. } = frame.payload {
            break request_id;
        }
    };
    handle.resolve(&request_id, Decision::AllowOnce).unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(2), gate)
        .await
        .unwrap()
        .unwrap());
    handle.interrupt();
}

/// LG-d: the watermark and the live buffers come from one read. A snapshot
/// taken after N frames reports seq N and carries exactly the buffers those
/// frames built; the next delta is the first frame above the watermark.
#[tokio::test]
async fn snapshot_point_pairs_the_watermark_with_the_live_buffers() {
    let (hub, id) = test_hub();
    let handle = hub.handle(&id).unwrap();
    handle.on_stream_delta(StreamDelta::Text("ab".into()));
    handle.on_stream_delta(StreamDelta::Text("cd".into()));
    let mut rx = handle.subscribe();

    let (watermark, live) = handle.snapshot_point();
    assert_eq!(watermark, 2);
    assert_eq!(live.streaming_text, "abcd");

    handle.on_stream_delta(StreamDelta::Text("e".into()));
    let next = parse_live(rx.recv().await.unwrap());
    assert_eq!(next.seq, watermark + 1, "first frame above the watermark");
    assert_eq!(handle.snapshot_point().1.streaming_text, "abcde");
}

/// LG-b: a pending delete fences out runs; a running session cannot be
/// fenced at all; cancelling the delete lifts the fence.
#[tokio::test]
async fn delete_fence_refuses_runs_until_it_settles() {
    let (hub, id) = test_hub();
    hub.begin_delete(id).unwrap();
    assert!(
        hub.start_run(id, parked_run(id)).is_err(),
        "no run may start while the delete is pending"
    );
    hub.cancel_delete(id);

    hub.start_run(id, parked_run(id)).unwrap();
    assert!(
        hub.begin_delete(id).is_err(),
        "a running session cannot be deleted"
    );
    assert!(hub.handle(&id).unwrap().interrupt());
}

/// LG-a: an accepted patch commits config, selector and labels together.
#[tokio::test]
async fn accepted_config_patch_moves_config_and_labels_together() {
    let (hub, id) = test_hub();
    let handle = hub.handle(&id).unwrap();
    handle
        .apply_config(&ConfigPatch {
            model: Some("other-model".into()),
            goal: Some("ship it".into()),
            ..ConfigPatch::default()
        })
        .unwrap();
    let live = handle.live_state();
    assert_eq!(live.model, "other-model");
    assert_eq!(live.goal, "ship it");
    assert_eq!(handle.engine_cell().config.default_model, "other-model");
}

fn rule_wire(effect: &str, spec: &str) -> RuleWire {
    RuleWire {
        effect: effect.into(),
        spec: spec.into(),
    }
}

/// The CLI's `[permissions]` reach a new session through the hub template:
/// the session's engine config carries the rules and live state reports
/// them as wire rules.
#[tokio::test]
async fn template_rules_reach_a_new_session() {
    let hub = SessionHub::empty();
    let store: Arc<dyn SessionStore> = Arc::new(modron_core::testing::InMemoryStore::new());
    let rules = RuleSet::new(vec![Rule::parse(RuleEffect::Deny, "bash(rm -rf*)").unwrap()]);
    hub.set_engine(Engine {
        selector: Arc::new(modron_core::testing::StaticSelector::new(
            "mock-model",
            "mock",
        )),
        providers: Default::default(),
        tools: Arc::new(modron_core::ToolRegistry::new()),
        store,
        config: EngineConfig {
            approvals: ApprovalPolicy {
                rules,
                ..ApprovalPolicy::default()
            },
            ..EngineConfig::default()
        },
    });
    let id = uuid::Uuid::new_v4();
    let handle = hub.ensure_session(id, None);
    assert_eq!(
        handle.live_state().rules,
        vec![rule_wire("deny", "bash(rm -rf*)")]
    );
    assert!(handle
        .engine_cell()
        .config
        .approvals
        .rules
        .evaluate("bash", &serde_json::json!({"command": "rm -rf x"}))
        .is_some());
}

/// A valid rules patch replaces the whole set for future runs and shows in
/// live state; an empty list clears it.
#[tokio::test]
async fn rules_patch_replaces_the_set_and_shows_in_live_state() {
    let (hub, id) = test_hub();
    let handle = hub.handle(&id).unwrap();
    assert!(handle.live_state().rules.is_empty());

    handle
        .apply_config(&ConfigPatch {
            rules: Some(vec![
                rule_wire("deny", "bash(rm -rf*)"),
                rule_wire("ask", "bash(git push*)"),
                rule_wire("allow", "read(src/**)"),
            ]),
            ..ConfigPatch::default()
        })
        .unwrap();
    assert_eq!(
        handle.live_state().rules,
        vec![
            rule_wire("deny", "bash(rm -rf*)"),
            rule_wire("ask", "bash(git push*)"),
            rule_wire("allow", "read(src/**)"),
        ]
    );

    // A second patch replaces the whole set, it does not merge.
    handle
        .apply_config(&ConfigPatch {
            rules: Some(vec![rule_wire("allow", "read")]),
            ..ConfigPatch::default()
        })
        .unwrap();
    assert_eq!(handle.live_state().rules, vec![rule_wire("allow", "read")]);

    handle
        .apply_config(&ConfigPatch {
            rules: Some(Vec::new()),
            ..ConfigPatch::default()
        })
        .unwrap();
    assert!(handle.live_state().rules.is_empty());
}

/// One invalid rule rejects the whole patch: the error names that rule, and
/// the rules, the model and everything else in the patch stay as they were.
#[tokio::test]
async fn invalid_rule_rejects_the_whole_patch_and_changes_nothing() {
    let (hub, id) = test_hub();
    let handle = hub.handle(&id).unwrap();
    handle
        .apply_config(&ConfigPatch {
            rules: Some(vec![rule_wire("deny", "bash(rm -rf*)")]),
            ..ConfigPatch::default()
        })
        .unwrap();
    let before = handle.live_state();

    let err = handle
        .apply_config(&ConfigPatch {
            model: Some("other-model".into()),
            rules: Some(vec![rule_wire("allow", "read"), rule_wire("deny", "bash(")]),
            ..ConfigPatch::default()
        })
        .expect_err("a bad rule rejects the patch");
    assert!(
        err.to_string().contains("bash("),
        "error names the rule: {err}"
    );

    let after = handle.live_state();
    assert_eq!(after.rules, before.rules);
    assert_eq!(after.model, before.model);
    assert_ne!(handle.engine_cell().config.default_model, "other-model");

    let bad_effect = handle
        .apply_config(&ConfigPatch {
            rules: Some(vec![rule_wire("maybe", "read")]),
            ..ConfigPatch::default()
        })
        .expect_err("an unknown effect is rejected");
    assert!(bad_effect.to_string().contains("maybe"));
}

/// ExplainRule's text: one line per rule, then `result:` with the verdict
/// the engine would reach. Deny beats the allow-all rule.
#[tokio::test]
async fn explain_rule_lists_each_rule_and_ends_with_the_result() {
    let (hub, id) = test_hub();
    let handle = hub.handle(&id).unwrap();
    handle
        .apply_config(&ConfigPatch {
            rules: Some(vec![
                rule_wire("allow", "bash"),
                rule_wire("deny", "bash(rm -rf*)"),
            ]),
            ..ConfigPatch::default()
        })
        .unwrap();

    let text = handle.explain_rule("bash", &serde_json::json!({"command": "rm -rf /"}));
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines,
        vec![
            "allow bash: matched",
            "deny bash(rm -rf*): matched",
            "result: deny",
        ]
    );

    let none = handle.explain_rule("read", &serde_json::json!({"path": "a"}));
    assert!(none.lines().last() == Some("result: no rule"), "{none}");
}

/// A provider for team runs: the plan call gets two items, every worker
/// answers `done` without touching the tree.
struct TeamMock;

#[async_trait::async_trait]
impl modron_core::ModelProvider for TeamMock {
    fn id(&self) -> &str {
        "mock"
    }
    async fn turn(
        &self,
        req: Arc<modron_core::TurnRequest>,
    ) -> modron_core::Result<modron_core::TurnResponse> {
        let text = if req.system.starts_with("You split a coding task") {
            r#"{"items":[{"task":"reply one"},{"task":"reply two"}]}"#
        } else {
            "done"
        };
        Ok(modron_core::TurnResponse {
            text: text.into(),
            tool_calls: Vec::new(),
            usage: Usage {
                input_tokens: 1,
                output_tokens: 1,
                cached_input_tokens: 0,
            },
            stop_reason: modron_core::StopReason::EndTurn,
            provider_blocks: Vec::new(),
        })
    }
}

/// Team stage notes are published to live subscribers as they are written:
/// the frames a subscriber saw, the persisted log and the transcript replay
/// all carry the same notes in the same order.
#[tokio::test]
async fn team_stage_notes_reach_live_subscribers_in_order() {
    let repo = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "hub@test.invalid"]);
    git(&["config", "user.name", "hub test"]);
    std::fs::write(repo.path().join("README.md"), "base\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "init"]);

    let hub = SessionHub::empty();
    let inner: Arc<dyn SessionStore> = Arc::new(modron_core::testing::InMemoryStore::new());
    let store: Arc<dyn SessionStore> =
        Arc::new(BroadcastingStore::new(Arc::clone(&inner), hub.clone()));
    let mut providers: std::collections::HashMap<String, Arc<dyn modron_core::ModelProvider>> =
        Default::default();
    providers.insert("mock".into(), Arc::new(TeamMock));
    let engine = Engine {
        selector: Arc::new(modron_core::testing::StaticSelector::new(
            "mock-model",
            "mock",
        )),
        providers,
        tools: Arc::new(modron_core::ToolRegistry::new()),
        store: Arc::clone(&store),
        config: EngineConfig {
            default_model: "mock-model".into(),
            default_provider: "mock".into(),
            working_dir: repo.path().to_path_buf(),
            ..EngineConfig::default()
        },
    };
    hub.set_engine(engine.clone());
    let meta = store.create_session("team").await.unwrap();
    let handle = hub.ensure_session(meta.id, None);
    // Subscribe before the run: the live frames are what a client sees.
    let mut rx = handle.subscribe();

    engine
        .run_task_resuming(meta.id, "/team 2 write two replies")
        .await
        .unwrap();

    let mut live = Vec::new();
    while let Ok(frame) = rx.try_recv() {
        let frame: Frame = serde_json::from_str(&frame.json).unwrap();
        if let Payload::Event {
            event: SessionEvent::Progress { text },
        } = frame.payload
        {
            live.push(text);
        }
    }
    let events = store.load(meta.id).await.unwrap();
    let persisted: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::Progress { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    let replayed: Vec<String> = modron_view::replay(&events)
        .into_iter()
        .filter_map(|entry| match entry {
            modron_view::TranscriptEntry::Note { text } => Some(text),
            _ => None,
        })
        .collect();

    let expected = vec![
        "team plan: 2 item(s)",
        "round 1 started: 2 item(s)",
        "round 1 merging 2 worker item(s)",
        "round 1 result: 2 merged, 0 conflict(s), 0 failed; verify not configured; review not configured",
    ];
    assert_eq!(live, expected, "live frames");
    assert_eq!(persisted, expected, "persisted log");
    assert_eq!(
        replayed
            .iter()
            .filter(|t| expected.contains(&t.as_str()))
            .count(),
        expected.len(),
        "replay carries every note"
    );
    let positions: Vec<usize> = expected
        .iter()
        .map(|note| replayed.iter().position(|t| t.as_str() == *note).unwrap())
        .collect();
    assert!(positions.windows(2).all(|w| w[0] < w[1]), "replay order");
}
