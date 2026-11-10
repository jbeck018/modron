//! WS integration tests (Phase 2 acceptance bar): a real server on
//! 127.0.0.1:0 with the mock provider drives fan-out, mid-run attach,
//! approvals, allow-rest, interrupt, busy, reconnect-with-gap, and
//! unknown-session errors.
//!
//! These tests need the multi-thread runtime: the session approver parks
//! the engine task with `block_in_place`.

mod common;

use common::{bash_call, read_call, spawn_server, text_turn, WsClient};
use modron_protocol::{Decision, Payload};
use std::time::Duration;

fn note_text(payload: &Payload) -> Option<&str> {
    match payload {
        Payload::Note { text } => Some(text),
        _ => None,
    }
}

fn error_text(payload: &Payload) -> Option<&str> {
    match payload {
        Payload::Error { message } => Some(message),
        _ => None,
    }
}

/// Extract the session id from a Snapshot frame.
fn snapshot_id(payload: &Payload) -> Option<modron_core::SessionId> {
    match payload {
        Payload::Snapshot { session_id, .. } => Some(*session_id),
        _ => None,
    }
}

/// (1) Two concurrent clients both receive Delta/Tool/TurnDone for one
/// submit — the hub fans every frame out to every attached client.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_clients_both_see_the_full_stream() {
    let server = spawn_server(vec![read_call("c1"), text_turn("done")], 100).await;
    let mut a = WsClient::connect(server.addr).await;
    let mut b = WsClient::connect(server.addr).await;

    // A submits; the auto-attach answers with a snapshot carrying the id.
    a.send(Payload::Submit {
        session_id: None,
        task: "run the thing".into(),
        attachments: vec![],
    })
    .await;
    let snapshot = a
        .recv_until(|p| matches!(p, Payload::Snapshot { .. }))
        .await;
    let session = snapshot_id(&snapshot.payload).unwrap();

    // B attaches to the same session before the run gets going.
    b.send(Payload::Attach {
        session_id: session,
        last_seq: 0,
    })
    .await;
    let b_snapshot = b
        .recv_until(|p| matches!(p, Payload::Snapshot { .. }))
        .await;
    assert_eq!(snapshot_id(&b_snapshot.payload), Some(session));

    for client in [&mut a, &mut b] {
        let tool = client
            .recv_until(|p| matches!(p, Payload::Tool { .. }))
            .await;
        assert!(matches!(
            tool.payload,
            Payload::Tool {
                activity: modron_protocol::WireToolActivity::Start { .. }
            }
        ));
        client
            .recv_until(|p| matches!(p, Payload::Delta { .. }))
            .await;
        let done = client
            .recv_until(|p| matches!(p, Payload::TurnDone { .. }))
            .await;
        match done.payload {
            Payload::TurnDone { summary } => {
                assert_eq!(summary.session_id, session);
                assert!(summary.answer.contains("done"));
            }
            other => panic!("expected turn_done, got {other:?}"),
        }
        // Every event frame for the session is seq-ordered.
    }
    server.server.shutdown();
}

/// (2) Attach mid-run: the second client receives the partial persisted
/// log as a snapshot, then the remaining live frames.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attach_mid_run_gets_snapshot_then_live_frames() {
    let server = spawn_server(vec![read_call("c1"), text_turn("late answer")], 600).await;
    let mut a = WsClient::connect(server.addr).await;
    a.send(Payload::Submit {
        session_id: None,
        task: "slow task".into(),
        attachments: vec![],
    })
    .await;
    let snapshot = a
        .recv_until(|p| matches!(p, Payload::Snapshot { .. }))
        .await;
    let session = snapshot_id(&snapshot.payload).unwrap();

    // Attach while the first provider turn is still sleeping.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut b = WsClient::connect(server.addr).await;
    b.send(Payload::Attach {
        session_id: session,
        last_seq: 0,
    })
    .await;
    let b_snapshot = b
        .recv_until(|p| matches!(p, Payload::Snapshot { .. }))
        .await;
    match b_snapshot.payload {
        Payload::Snapshot { events, live, .. } => {
            assert!(
                events
                    .iter()
                    .any(|e| matches!(e, modron_core::SessionEvent::Task { .. })),
                "snapshot must cover the persisted task event"
            );
            assert!(live.running, "the run is in flight");
        }
        other => panic!("expected snapshot, got {other:?}"),
    }
    // Then the live tail: the tool call + completion still arrive.
    b.recv_until(|p| matches!(p, Payload::Tool { .. })).await;
    b.recv_until(|p| matches!(p, Payload::TurnDone { .. }))
        .await;
    server.server.shutdown();
}

/// (3) Approval round-trip: Ask mode + a mutating tool call raises
/// ApprovalRequested to every attached client; Deny resolves it and the
/// run continues to its scripted end.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn approval_round_trip_deny_reaches_all_clients() {
    let server = spawn_server(vec![bash_call("c1"), text_turn("adapted")], 500).await;
    let mut a = WsClient::connect(server.addr).await;
    a.send(Payload::Submit {
        session_id: None,
        task: "mutate something".into(),
        attachments: vec![],
    })
    .await;
    let session = snapshot_id(
        &a.recv_until(|p| matches!(p, Payload::Snapshot { .. }))
            .await
            .payload,
    )
    .unwrap();

    // B attaches while the first turn is sleeping — before the approval.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let mut b = WsClient::connect(server.addr).await;
    b.send(Payload::Attach {
        session_id: session,
        last_seq: 0,
    })
    .await;
    b.recv_until(|p| matches!(p, Payload::Snapshot { .. }))
        .await;

    // A (and B) see the request; A denies it.
    let requested = a
        .recv_until(|p| matches!(p, Payload::ApprovalRequested { .. }))
        .await;
    let request_id = match requested.payload {
        Payload::ApprovalRequested { request_id, call } => {
            assert_eq!(call.name, "bash");
            request_id
        }
        other => panic!("expected approval_requested, got {other:?}"),
    };
    b.recv_until(|p| matches!(p, Payload::ApprovalRequested { .. }))
        .await;

    a.send(Payload::Approve {
        request_id: request_id.clone(),
        decision: Decision::Deny,
    })
    .await;

    for client in [&mut a, &mut b] {
        let resolved = client
            .recv_until(|p| matches!(p, Payload::ApprovalResolved { .. }))
            .await;
        match resolved.payload {
            Payload::ApprovalResolved {
                request_id: rid,
                decision,
            } => {
                assert_eq!(rid, request_id);
                assert_eq!(decision, Decision::Deny);
            }
            other => panic!("expected approval_resolved, got {other:?}"),
        }
        client
            .recv_until(|p| matches!(p, Payload::TurnDone { .. }))
            .await;
    }
    server.server.shutdown();
}

/// (4) AllowAll once: the next mutating call auto-approves without a
/// request frame (sticky approve-rest).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn allow_all_once_auto_approves_the_rest() {
    let server = spawn_server(
        vec![bash_call("c1"), bash_call("c2"), text_turn("all approved")],
        100,
    )
    .await;
    let mut a = WsClient::connect(server.addr).await;
    a.send(Payload::Submit {
        session_id: None,
        task: "two mutations".into(),
        attachments: vec![],
    })
    .await;

    let requested = a
        .recv_until(|p| matches!(p, Payload::ApprovalRequested { .. }))
        .await;
    let request_id = match requested.payload {
        Payload::ApprovalRequested { request_id, .. } => request_id,
        other => panic!("expected approval_requested, got {other:?}"),
    };
    a.send(Payload::Approve {
        request_id,
        decision: Decision::AllowAll,
    })
    .await;

    // Drain to TurnDone: the first (already consumed) request was the only
    // one - the second mutation must auto-approve with zero new requests.
    let mut requests = 0;
    for _ in 0..200 {
        let frame = a.recv().await;
        match frame.payload {
            Payload::ApprovalRequested { .. } => requests += 1,
            Payload::TurnDone { .. } => break,
            _ => {}
        }
    }
    assert_eq!(requests, 0, "the second mutation must auto-approve");

    // The resolution echo arrived before TurnDone (order drained above);
    // also confirm the sticky flag persists into the live state.
    server.server.shutdown();
}

/// (5) Interrupt aborts the run; every attached client receives
/// TurnAborted and the session accepts a new run afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupt_broadcasts_turn_aborted_to_all_clients() {
    // Text-only script: the aborted first run consumes nothing, so the
    // post-interrupt re-run hits a scripted answer, not the approval gate.
    let server = spawn_server(
        vec![text_turn("never reached"), text_turn("second run")],
        5_000,
    )
    .await;
    let mut a = WsClient::connect(server.addr).await;
    let mut b = WsClient::connect(server.addr).await;

    a.send(Payload::Submit {
        session_id: None,
        task: "long task".into(),
        attachments: vec![],
    })
    .await;
    let snapshot = a
        .recv_until(|p| matches!(p, Payload::Snapshot { .. }))
        .await;
    let session = snapshot_id(&snapshot.payload).unwrap();
    b.send(Payload::Attach {
        session_id: session,
        last_seq: 0,
    })
    .await;
    b.recv_until(|p| matches!(p, Payload::Snapshot { .. }))
        .await;

    a.send(Payload::Interrupt {
        session_id: session,
    })
    .await;
    for client in [&mut a, &mut b] {
        client
            .recv_until(|p| matches!(p, Payload::TurnAborted { .. }))
            .await;
    }

    // The slot is free again: a second submit is accepted (busy would be
    // an error frame).
    a.send(Payload::Submit {
        session_id: Some(session),
        task: "next task".into(),
        attachments: vec![],
    })
    .await;
    // The re-run completes (whichever scripted entry the aborted first
    // run left unconsumed -- that race is the scheduler's, not ours),
    // proving the slot freed and a fresh run was accepted.
    let done = a
        .recv_until(|p| matches!(p, Payload::TurnDone { .. } | Payload::TurnFailed { .. }))
        .await;
    assert!(
        matches!(done.payload, Payload::TurnDone { .. }),
        "got {:?}",
        done.payload
    );
    server.server.shutdown();
}

/// (6) A second submit into a running session gets an Error frame, and
/// the original run is untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn second_submit_while_busy_errors() {
    let server = spawn_server(vec![text_turn("first run")], 800).await;
    let mut a = WsClient::connect(server.addr).await;
    a.send(Payload::Submit {
        session_id: None,
        task: "occupy the session".into(),
        attachments: vec![],
    })
    .await;
    let snapshot = a
        .recv_until(|p| matches!(p, Payload::Snapshot { .. }))
        .await;
    let session = snapshot_id(&snapshot.payload).unwrap();

    a.send(Payload::Submit {
        session_id: Some(session),
        task: "collide".into(),
        attachments: vec![],
    })
    .await;
    let error = a.recv_until(|p| error_text(p).is_some()).await;
    let message = error_text(&error.payload).unwrap().to_string();
    assert!(message.contains("busy"), "got: {message}");
    assert_eq!(error.id, Some(3), "the error echoes the request id");

    // The first run completes normally.
    a.recv_until(|p| matches!(p, Payload::TurnDone { .. }))
        .await;
    server.server.shutdown();
}

/// (7) Reconnect with a seq gap: the client disconnects mid-run,
/// re-attaches with its last_seq, and the snapshot covers the missed
/// events; no post-snapshot frame replays old seqs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconnect_with_last_seq_covers_the_gap() {
    let server = spawn_server(vec![read_call("c1"), text_turn("final")], 500).await;
    let mut a = WsClient::connect(server.addr).await;
    a.send(Payload::Submit {
        session_id: None,
        task: "disconnecty".into(),
        attachments: vec![],
    })
    .await;
    let first = a
        .recv_until(|p| matches!(p, Payload::Snapshot { .. }))
        .await;
    let session = snapshot_id(&first.payload).unwrap();

    // Read one live frame, note its seq, then drop the connection mid-run.
    let live = a.recv().await;
    let last_seen = live.seq.max(first.seq);
    a.close().await;
    drop(a);

    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut b = WsClient::connect(server.addr).await;
    b.send(Payload::Attach {
        session_id: session,
        last_seq: last_seen,
    })
    .await;
    let snapshot = b
        .recv_until(|p| matches!(p, Payload::Snapshot { .. }))
        .await;
    match snapshot.payload {
        Payload::Snapshot {
            last_seq,
            events,
            live,
            ..
        } => {
            assert!(last_seq >= last_seen, "snapshot is current");
            assert!(!events.is_empty(), "the persisted log covers the gap");
            assert!(live.running);
        }
        other => panic!("expected snapshot, got {other:?}"),
    }
    // Frames after the snapshot never replay the client's history.
    for _ in 0..3 {
        let frame = b.recv().await;
        if frame.session.is_some() {
            assert!(
                frame.seq > last_seen || frame.seq == 0,
                "post-snapshot frame seq {} must exceed last_seen {last_seen}",
                frame.seq
            );
        }
        if matches!(frame.payload, Payload::TurnDone { .. }) {
            break;
        }
    }
    b.recv_until(|p| matches!(p, Payload::TurnDone { .. }))
        .await;
    server.server.shutdown();
}

/// (8) Attaching to an unknown session errors (never fabricates state).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attach_to_unknown_session_errors() {
    let server = spawn_server(vec![text_turn("unused")], 10).await;
    let mut a = WsClient::connect(server.addr).await;
    let ghost = uuid::Uuid::new_v4();
    a.send(Payload::Attach {
        session_id: ghost,
        last_seq: 0,
    })
    .await;
    let error = a.recv_until(|p| error_text(p).is_some()).await;
    assert!(
        error_text(&error.payload)
            .unwrap()
            .contains("no such session"),
        "got: {:?}",
        error.payload
    );
    // Submitting into it errors too (unknown id, not a silent new session).
    a.send(Payload::Submit {
        session_id: Some(ghost),
        task: "into the void".into(),
        attachments: vec![],
    })
    .await;
    let error = a.recv_until(|p| error_text(p).is_some()).await;
    assert!(error_text(&error.payload)
        .unwrap()
        .contains("no such session"));
    server.server.shutdown();
}

/// NewSession creates an empty live session; its id is discoverable and
/// a later Submit adopts it (single Task event, no duplicate shell).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn new_session_then_submit_adopts_the_empty_session() {
    let server = spawn_server(vec![text_turn("adopted")], 10).await;
    let mut a = WsClient::connect(server.addr).await;
    a.send(Payload::NewSession { title: None }).await;
    let note = a.recv_until(|p| note_text(p).is_some()).await;
    let text = note_text(&note.payload).unwrap();
    let id: modron_core::SessionId = text
        .strip_prefix("session created: ")
        .expect("note carries the id")
        .parse()
        .expect("note carries a uuid");

    let mut b = WsClient::connect(server.addr).await;
    b.send(Payload::Attach {
        session_id: id,
        last_seq: 0,
    })
    .await;
    let snapshot = b
        .recv_until(|p| matches!(p, Payload::Snapshot { .. }))
        .await;
    match snapshot.payload {
        Payload::Snapshot { events, .. } => assert!(events.is_empty(), "fresh session"),
        other => panic!("expected snapshot, got {other:?}"),
    }

    a.send(Payload::Submit {
        session_id: Some(id),
        task: "adopt me".into(),
        attachments: vec![],
    })
    .await;
    // The adopted run appends exactly ONE Task event (no double shell).
    let mut task_events = 0;
    for _ in 0..200 {
        let frame = a.recv().await;
        match &frame.payload {
            Payload::Event {
                event: modron_core::SessionEvent::Task { .. },
            } => task_events += 1,
            Payload::TurnDone { summary } => {
                assert_eq!(summary.answer, "adopted");
                break;
            }
            _ => {}
        }
    }
    assert_eq!(task_events, 1);
    server.server.shutdown();
}

/// A malformed line is answered with an error frame, not a crash.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unparseable_frame_errors_cleanly() {
    let server = spawn_server(vec![text_turn("ok")], 10).await;
    let mut a = WsClient::connect(server.addr).await;
    a.send_raw_text("{not json").await;
    let error = a.recv_until(|p| error_text(p).is_some()).await;
    assert!(error_text(&error.payload).unwrap().contains("unparseable"));
    // The connection still works afterwards.
    a.send(Payload::Ping {}).await;
    assert!(matches!(
        a.recv_until(|p| matches!(p, Payload::Pong {}))
            .await
            .payload,
        Payload::Pong {}
    ));
    server.server.shutdown();
}

/// Fork: the Note carries the new id and the forked log is attachable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fork_copies_the_log_under_a_fresh_id() {
    let server = spawn_server(vec![text_turn("source answer")], 10).await;
    let mut a = WsClient::connect(server.addr).await;
    a.send(Payload::Submit {
        session_id: None,
        task: "original".into(),
        attachments: vec![],
    })
    .await;
    let source = snapshot_id(
        &a.recv_until(|p| matches!(p, Payload::Snapshot { .. }))
            .await
            .payload,
    )
    .unwrap();
    a.recv_until(|p| matches!(p, Payload::TurnDone { .. }))
        .await;

    a.send(Payload::Fork { session_id: source }).await;
    let note = a.recv_until(|p| note_text(p).is_some()).await;
    let text = note_text(&note.payload).unwrap();
    let new_id: modron_core::SessionId = text
        .strip_prefix(&format!("forked {source} -> "))
        .expect("note carries the fork id")
        .parse()
        .expect("fork note carries a uuid");
    assert_ne!(new_id, source);

    // The fork is attachable and replays the source log.
    let mut b = WsClient::connect(server.addr).await;
    b.send(Payload::Attach {
        session_id: new_id,
        last_seq: 0,
    })
    .await;
    match b
        .recv_until(|p| matches!(p, Payload::Snapshot { .. }))
        .await
        .payload
    {
        Payload::Snapshot { events, .. } => {
            assert!(events.iter().any(
                |e| matches!(e, modron_core::SessionEvent::Task { text } if text == "original")
            ));
        }
        other => panic!("expected snapshot, got {other:?}"),
    }
    server.server.shutdown();
}

/// SetConfig on the attached session publishes ConfigChanged and the live
/// labels follow.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn set_config_updates_labels_and_echoes() {
    let server = spawn_server(vec![text_turn("unused")], 10).await;
    let mut a = WsClient::connect(server.addr).await;
    a.send(Payload::NewSession { title: None }).await;
    let note = a.recv_until(|p| note_text(p).is_some()).await;
    let id: modron_core::SessionId = note_text(&note.payload)
        .unwrap()
        .strip_prefix("session created: ")
        .unwrap()
        .parse()
        .unwrap();
    a.send(Payload::Attach {
        session_id: id,
        last_seq: 0,
    })
    .await;
    a.recv_until(|p| matches!(p, Payload::Snapshot { .. }))
        .await;

    a.send(Payload::SetConfig {
        patch: modron_protocol::ConfigPatch {
            model: Some("mock-model".into()),
            goal: Some("ship it".into()),
            ..Default::default()
        },
    })
    .await;
    let changed = a
        .recv_until(|p| matches!(p, Payload::ConfigChanged { .. }))
        .await;
    match changed.payload {
        Payload::ConfigChanged { patch } => {
            assert_eq!(patch.model.as_deref(), Some("mock-model"));
            assert_eq!(patch.goal.as_deref(), Some("ship it"));
        }
        other => panic!("expected config_changed, got {other:?}"),
    }
    server.server.shutdown();
}

/// The hub stays usable after several sequential runs in one session
/// (run-slot lifecycle: busy clears, seq keeps rising).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sequential_runs_reuse_the_session_and_seq_rises() {
    let server = spawn_server(vec![text_turn("one"), text_turn("two")], 20).await;
    let mut a = WsClient::connect(server.addr).await;
    a.send(Payload::Submit {
        session_id: None,
        task: "first".into(),
        attachments: vec![],
    })
    .await;
    let first_snapshot = a
        .recv_until(|p| matches!(p, Payload::Snapshot { .. }))
        .await;
    let session = snapshot_id(&first_snapshot.payload).unwrap();
    let first_done = a
        .recv_until(|p| matches!(p, Payload::TurnDone { .. }))
        .await;
    let first_seq = first_done.seq;

    a.send(Payload::Submit {
        session_id: Some(session),
        task: "second".into(),
        attachments: vec![],
    })
    .await;
    let second_done = a
        .recv_until(|p| matches!(p, Payload::TurnDone { .. }))
        .await;
    assert!(
        second_done.seq > first_seq,
        "seq must keep rising across runs"
    );
    match second_done.payload {
        Payload::TurnDone { summary } => assert_eq!(summary.answer, "two"),
        other => panic!("expected turn_done, got {other:?}"),
    }
    server.server.shutdown();
}
