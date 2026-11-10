//! Frame dispatch — the shared server-side handler for both transports
//! (one JSON [`Frame`] per WebSocket text message or Unix-socket NDJSON
//! line). One connection at a time: a connection holds at most one live
//! subscription (re-`attach` switches, `detach` drops it).
//!
//! Attach ordering (get it right): subscribe to the session broadcast
//! channel FIRST and start buffering live frames, THEN load the persisted
//! log and send the Snapshot, and only then forward the buffered frames.
//! A client therefore never misses a frame between snapshot and live;
//! duplicates (events both in the log and buffered) are dropped client-side
//! by `seq`.

use std::path::PathBuf;
use std::sync::Arc;

use modron_core::SessionId;
use modron_protocol::{Frame, Payload, SessionMetaWire, PROTOCOL_VERSION};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::hub::{session_title, task_with_goal, RunFuture, SessionHub};
use crate::pairing;

/// Everything a connection's frames need.
pub struct ServerState {
    pub hub: SessionHub,
    /// The shared (broadcasting) store: list/load/fork/create paths.
    pub store: Arc<dyn modron_core::SessionStore>,
    /// Device-token directory for remote authentication.
    pub devices_dir: PathBuf,
    /// Advertised in `welcome`.
    pub version: String,
}

/// The live frames of one subscription: the broadcast->buffer forwarder
/// plus the buffer->socket pump. Aborting the pump detaches.
pub struct Subscription {
    pub session: SessionId,
    forward: JoinHandle<()>,
    pump: JoinHandle<()>,
}

/// Per-connection state.
pub struct ConnCtx {
    /// Server->client frames (the transport's writer task drains it).
    pub out: mpsc::UnboundedSender<Frame>,
    /// Set by the `hello` handshake.
    pub authed: bool,
    pub subscription: Option<Subscription>,
}

impl ConnCtx {
    pub fn new(out: mpsc::UnboundedSender<Frame>) -> Self {
        Self {
            out,
            authed: false,
            subscription: None,
        }
    }
}

impl Drop for ConnCtx {
    fn drop(&mut self) {
        if let Some(sub) = self.subscription.take() {
            sub.pump.abort();
            sub.forward.abort();
        }
    }
}

/// Whether the connection handler should continue reading frames.
#[derive(PartialEq)]
pub enum Flow {
    Continue,
    Close,
}

fn reply(id: Option<u64>, session: Option<SessionId>, payload: Payload) -> Frame {
    Frame {
        id,
        session,
        seq: 0,
        payload,
    }
}

fn error(frame_id: Option<u64>, message: impl Into<String>) -> Frame {
    reply(
        frame_id,
        None,
        Payload::Error {
            message: message.into(),
        },
    )
}

/// Fire-and-forget send to one connection (a closed transport just drops
/// the frame).
pub(crate) fn send(ctx: &ConnCtx, frame: Frame) {
    let _ = ctx.out.send(frame);
}

/// Handle one client frame. Returns [`Flow::Close`] on auth failures.
pub async fn handle_frame(
    state: &Arc<ServerState>,
    ctx: &mut ConnCtx,
    trusted: bool,
    frame: Frame,
) -> Flow {
    let request_id = frame.id;
    match frame.payload {
        Payload::Hello {
            protocol, token, ..
        } => {
            if protocol != PROTOCOL_VERSION {
                send(ctx, error(
                    request_id,
                    format!(
                        "protocol mismatch: server speaks v{PROTOCOL_VERSION}, client sent v{protocol}"
                    ),
                ));
                return Flow::Close;
            }
            if let Err(message) =
                pairing::check_token(trusted, token.as_deref(), &state.devices_dir)
            {
                send(ctx, error(request_id, message));
                return Flow::Close;
            }
            ctx.authed = true;
            send(
                ctx,
                reply(
                    request_id,
                    None,
                    Payload::Welcome {
                        server_version: state.version.clone(),
                        protocol: PROTOCOL_VERSION,
                    },
                ),
            );
            return Flow::Continue;
        }
        _ if !ctx.authed => {
            send(ctx, error(request_id, "send a hello frame first"));
            return Flow::Close;
        }
        Payload::Ping {} => {
            send(ctx, reply(request_id, None, Payload::Pong {}));
            return Flow::Continue;
        }
        Payload::ListSessions {} => match state.store.list().await {
            Ok(sessions) => {
                let rows = sessions
                    .into_iter()
                    .map(|meta| SessionMetaWire {
                        id: meta.id,
                        title: meta.title,
                        created_at: meta.created_at.to_rfc3339(),
                        updated_at: meta.updated_at.to_rfc3339(),
                        message_count: meta.message_count,
                    })
                    .collect();
                send(
                    ctx,
                    reply(request_id, None, Payload::Sessions { sessions: rows }),
                );
            }
            Err(e) => send(ctx, error(request_id, e.to_string())),
        },
        Payload::NewSession { title } => match state
            .store
            .create_session(title.as_deref().unwrap_or("remote session"))
            .await
        {
            Ok(meta) => {
                state.hub.ensure_session(meta.id, Some(meta.id));
                // Direct, correlated reply: the requester cannot be
                // subscribed to a session it just created.
                send(
                    ctx,
                    reply(
                        request_id,
                        Some(meta.id),
                        Payload::Note {
                            text: format!("session created: {}", meta.id),
                        },
                    ),
                );
            }
            Err(e) => send(ctx, error(request_id, e.to_string())),
        },
        Payload::Submit {
            session_id,
            task,
            attachments,
        } => {
            submit(state, ctx, request_id, session_id, task, attachments).await;
        }
        Payload::Interrupt { session_id } => {
            match state.hub.handle(&session_id) {
                Some(handle) => {
                    if !handle.interrupt() {
                        send(ctx, error(request_id, "no active run in this session"));
                    }
                    // Success announces itself as TurnAborted.
                }
                None => send(
                    ctx,
                    error(request_id, format!("no such session {session_id}")),
                ),
            }
        }
        Payload::Approve {
            request_id: req,
            decision,
        } => match state.hub.resolve_any(&req, decision) {
            Ok(_) => {}
            Err(message) => send(ctx, error(request_id, message)),
        },
        Payload::SetConfig { patch } => {
            let Some(sub) = &ctx.subscription else {
                send(
                    ctx,
                    error(request_id, "attach to a session before changing its config"),
                );
                return Flow::Continue;
            };
            let Some(handle) = state.hub.handle(&sub.session) else {
                send(ctx, error(request_id, "session is no longer live"));
                return Flow::Continue;
            };
            match handle.apply_config(&patch) {
                Ok(()) => handle.publish(Payload::ConfigChanged { patch }),
                Err(e) => send(ctx, error(request_id, e.to_string())),
            }
        }
        Payload::Fork { session_id } => {
            // Validate the source first: an unknown id must be an error,
            // not a silently empty fork.
            match state.store.load(session_id).await {
                Ok(events) if events.is_empty() => {
                    send(
                        ctx,
                        error(
                            request_id,
                            format!("session {session_id} has no events to fork"),
                        ),
                    );
                }
                Ok(_) => match state.store.fork(session_id).await {
                    Ok(new_id) => {
                        state.hub.ensure_session(new_id, None);
                        let note = Payload::Note {
                            text: format!("forked {session_id} -> {new_id}"),
                        };
                        // Both channels: the source session's clients see
                        // the fork happen, and the requester gets the new
                        // id even when not attached.
                        if let Some(handle) = state.hub.handle(&session_id) {
                            handle.publish(note.clone());
                        }
                        send(ctx, reply(request_id, Some(session_id), note));
                    }
                    Err(e) => send(ctx, error(request_id, e.to_string())),
                },
                Err(e) => send(ctx, error(request_id, e.to_string())),
            }
        }
        Payload::Attach { session_id, .. } => {
            attach(state, ctx, request_id, session_id).await;
        }
        Payload::Detach {} => {
            if let Some(sub) = ctx.subscription.take() {
                sub.pump.abort();
                sub.forward.abort();
            }
        }
        // Server->client payloads arriving at the server are protocol misuse.
        other => {
            send(
                ctx,
                error(
                    request_id,
                    format!(
                        "frames of type `{}` are server-to-client",
                        variant_name(&other)
                    ),
                ),
            );
        }
    }
    Flow::Continue
}

/// Submit a task. `session_id: None` reserves a fresh session (the id
/// reaches the client on the first seq'd frame); `Some(id)` resumes (or
/// adopts an empty session created via `new_session`).
async fn submit(
    state: &Arc<ServerState>,
    ctx: &mut ConnCtx,
    request_id: Option<u64>,
    session_id: Option<SessionId>,
    task: String,
    attachments: Vec<modron_core::Attachment>,
) {
    if task.trim().is_empty() {
        send(ctx, error(request_id, "task text is empty"));
        return;
    }
    let (handle, run): (_, RunFuture) = match session_id {
        None => {
            // Reserve the session now so the hub handle exists before any
            // frame flows; the run's create call adopts it (pinned store).
            let meta = match state.store.create_session(&session_title(&task)).await {
                Ok(meta) => meta,
                Err(e) => {
                    send(ctx, error(request_id, e.to_string()));
                    return;
                }
            };
            let handle = state.hub.ensure_session(meta.id, Some(meta.id));
            let engine = handle.engine_cell().clone();
            let run =
                Box::pin(async move { engine.run_task_with_attachments(&task, attachments).await });
            (handle, run)
        }
        Some(id) => {
            // Known session: resume when the log has events; adopt (pinned
            // run) when it is an empty shell from `new_session`. An unknown
            // id is an error, never a silent new session.
            let events = match state.store.load(id).await {
                Ok(events) => events,
                Err(e) => {
                    send(ctx, error(request_id, format!("no such session {id}: {e}")));
                    return;
                }
            };
            let handle = state.hub.ensure_session(id, Some(id));
            let engine = handle.engine_cell().clone();
            let goal = handle_goal(&handle);
            let full_task = task_with_goal(&goal, &task);
            let run: RunFuture = if events.is_empty() {
                let task = full_task;
                Box::pin(async move { engine.run_task_with_attachments(&task, attachments).await })
            } else {
                Box::pin(async move {
                    engine
                        .run_task_resuming_with_attachments(id, &full_task, attachments)
                        .await
                })
            };
            (handle, run)
        }
    };
    if let Err(message) = state.hub.start_run(handle.id, run) {
        send(ctx, error(request_id, message));
        return;
    }
    // Submitting implies watching: a connection with no subscription is
    // attached to the session it just started (otherwise the assigned id
    // would never reach the submitter — there is no submit-ack frame).
    if ctx.subscription.is_none() {
        attach(state, ctx, None, handle.id).await;
    }
}

/// Read the handle's standing goal label.
fn handle_goal(handle: &crate::hub::SessionHandle) -> String {
    handle.live_state().goal
}

/// Attach: subscribe first (buffering live frames), then snapshot, then
/// forward the buffer. Clients dedupe by seq.
async fn attach(
    state: &Arc<ServerState>,
    ctx: &mut ConnCtx,
    request_id: Option<u64>,
    session_id: SessionId,
) {
    // Detach any previous subscription (one live subscription per
    // connection; re-attach switches).
    if let Some(old) = ctx.subscription.take() {
        old.pump.abort();
        old.forward.abort();
    }
    // The session must exist somewhere: live in the hub or on disk.
    let handle = match state.hub.handle(&session_id) {
        Some(handle) => handle,
        None => {
            // Known on disk? Unknown ids stay an error even though ensure
            // would happily fabricate a handle.
            match state.store.load(session_id).await {
                Ok(_) => {}
                Err(e) => {
                    send(
                        ctx,
                        error(request_id, format!("no such session {session_id}: {e}")),
                    );
                    return;
                }
            }
            state.hub.ensure_session(session_id, Some(session_id))
        }
    };

    // 1. Subscribe BEFORE loading anything, buffering live frames.
    let rx = handle.subscribe();
    let (buf_tx, buf_rx) = mpsc::unbounded_channel::<Frame>();
    let forward = tokio::spawn(async move {
        let mut rx = rx;
        loop {
            match rx.recv().await {
                Ok(frame) => {
                    if buf_tx.send(frame).is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "client lagged the session stream");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // 2. Load the persisted log + live state; send the snapshot first.
    let events = match state.store.load(session_id).await {
        Ok(events) => events,
        Err(e) => {
            forward.abort();
            send(ctx, error(request_id, e.to_string()));
            return;
        }
    };
    let snapshot = Frame {
        id: request_id,
        session: Some(session_id),
        seq: handle.current_seq(),
        payload: Payload::Snapshot {
            session_id,
            last_seq: handle.current_seq(),
            events,
            live: handle.live_state(),
        },
    };
    if ctx.out.send(snapshot).is_err() {
        forward.abort();
        return;
    }

    // 3. Forward the buffered live frames (clients dedupe by seq).
    let out = ctx.out.clone();
    let pump = tokio::spawn(async move {
        let mut buf_rx = buf_rx;
        while let Some(frame) = buf_rx.recv().await {
            if out.send(frame).is_err() {
                break;
            }
        }
    });
    ctx.subscription = Some(Subscription {
        session: session_id,
        forward,
        pump,
    });
}

/// The variant name for misuse diagnostics (no Debug on Payload, so read
/// the serde tag shape instead).
fn variant_name(payload: &Payload) -> &'static str {
    match payload {
        Payload::Welcome { .. } => "welcome",
        Payload::Snapshot { .. } => "snapshot",
        Payload::Event { .. } => "event",
        Payload::Delta { .. } => "delta",
        Payload::Tool { .. } => "tool",
        Payload::ApprovalRequested { .. } => "approval_requested",
        Payload::ApprovalResolved { .. } => "approval_resolved",
        Payload::ConfigChanged { .. } => "config_changed",
        Payload::Sessions { .. } => "sessions",
        Payload::TurnDone { .. } => "turn_done",
        Payload::TurnFailed { .. } => "turn_failed",
        Payload::TurnAborted {} => "turn_aborted",
        Payload::OAuthProgress { .. } => "oauth_progress",
        Payload::Note { .. } => "note",
        Payload::Pong {} => "pong",
        Payload::Error { .. } => "error",
        Payload::Hello { .. } => "hello",
        Payload::Attach { .. } => "attach",
        Payload::Detach {} => "detach",
        Payload::Submit { .. } => "submit",
        Payload::Interrupt { .. } => "interrupt",
        Payload::Approve { .. } => "approve",
        Payload::SetConfig { .. } => "set_config",
        Payload::Fork { .. } => "fork",
        Payload::NewSession { .. } => "new_session",
        Payload::ListSessions {} => "list_sessions",
        Payload::Ping {} => "ping",
    }
}
