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

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use modron_core::{Phase, SessionId};
use modron_protocol::{
    ConfigPatch, Frame, ModelWire, Payload, PlanDecision, SessionMetaWire, PROTOCOL_VERSION,
};
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinHandle;

use crate::hub::{
    session_title, task_with_goal, RunFuture, RunStarter, SessionHub, NO_PLAN_PENDING,
};
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
    /// The repo the server serves (git worktree tasks run here).
    pub working_dir: PathBuf,
    /// Worktree-task registry: session id -> task record.
    pub tasks: crate::tasks::TaskRegistry,
    /// Configured remote-control bridge base (scheme + host), if any.
    pub bridge_base: Option<String>,
    /// Session -> share key (minted by /remote-control; used to auth
    /// page publishes).
    pub rc_keys: std::sync::Mutex<HashMap<SessionId, String>>,
    /// Session -> the live tunnel's abort handle: re-registration aborts
    /// the previous tunnel instead of stacking duplicate pumpers
    /// (2026-11-12 cycle-2 audit L7).
    pub rc_tunnels: std::sync::Mutex<HashMap<SessionId, tokio::task::AbortHandle>>,
}

/// The live frames of one subscription: the broadcast->buffer forwarder
/// plus the buffer->socket pump. Aborting the pump detaches.
pub struct Subscription {
    pub session: SessionId,
    forward: JoinHandle<()>,
    pump: JoinHandle<()>,
}

/// What a connection's writer sends: per-connection replies (serialized
/// here) or a session broadcast (JSON already serialized once at publish).
#[derive(Debug)]
pub enum Outbound {
    Reply(Box<Frame>),
    Live(Box<crate::hub::LiveFrame>),
}

/// Queued-but-unwritten frame accounting for one capped connection (the
/// WebSocket transport). The sender side counts up in [`enqueue_on`]; the
/// writer task counts down through [`Backlog::written`] after each write
/// attempt. Past `limit` the connection is closed: the overflow signal fires
/// and the transport ends the connection (LG-f).
pub struct Backlog {
    queued: AtomicUsize,
    limit: usize,
    warned: AtomicBool,
    overflow: Notify,
}

impl Backlog {
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            queued: AtomicUsize::new(0),
            limit,
            warned: AtomicBool::new(false),
            overflow: Notify::new(),
        })
    }

    /// Writer side: one queued frame has been written (or given up on).
    pub fn written(&self) {
        self.queued.fetch_sub(1, Ordering::AcqRel);
    }

    /// Fires (and stays signalled) once the backlog has overflowed.
    pub fn overflow(&self) -> &Notify {
        &self.overflow
    }
}

/// Enqueue one outbound frame on a connection's channel, counting it
/// against `backlog` when the connection is capped. Returns `false` when
/// the frame was not queued (channel closed, or the backlog overflowed and
/// the connection is being closed).
fn enqueue_on(
    out: &mpsc::UnboundedSender<Outbound>,
    backlog: Option<&Backlog>,
    outbound: Outbound,
) -> bool {
    if let Some(backlog) = backlog {
        let queued = backlog.queued.fetch_add(1, Ordering::AcqRel) + 1;
        if queued > backlog.limit {
            backlog.queued.fetch_sub(1, Ordering::AcqRel);
            if !backlog.warned.swap(true, Ordering::AcqRel) {
                tracing::warn!(
                    limit = backlog.limit,
                    "client backlog over the cap; closing the connection"
                );
            }
            backlog.overflow.notify_one();
            return false;
        }
    }
    if out.send(outbound).is_ok() {
        true
    } else {
        if let Some(backlog) = backlog {
            backlog.queued.fetch_sub(1, Ordering::AcqRel);
        }
        false
    }
}

/// Per-connection state.
pub struct ConnCtx {
    /// Server->client frames (the transport's writer task drains it).
    pub out: mpsc::UnboundedSender<Outbound>,
    /// Backlog accounting when the transport caps its queue (WebSocket);
    /// `None` for the uncapped transports.
    pub backlog: Option<Arc<Backlog>>,
    /// Set by the `hello` handshake.
    pub authed: bool,
    pub subscription: Option<Subscription>,
    /// `Some(id)` pins the connection to one session (a remote-control
    /// tunnel, decision D5): only the allowlisted frames are served, and
    /// every session-addressed frame must name `id`. `None` is an ordinary
    /// connection, unchanged.
    pub scope: Option<SessionId>,
}

impl ConnCtx {
    pub fn new(out: mpsc::UnboundedSender<Outbound>) -> Self {
        Self {
            out,
            backlog: None,
            authed: false,
            subscription: None,
            scope: None,
        }
    }

    /// A connection whose queue is capped by `backlog` (the WebSocket
    /// writer must decrement the same counter).
    pub fn with_backlog(out: mpsc::UnboundedSender<Outbound>, backlog: Arc<Backlog>) -> Self {
        Self {
            out,
            backlog: Some(backlog),
            authed: false,
            subscription: None,
            scope: None,
        }
    }

    /// A connection already authenticated by construction (the tunnel's
    /// key was minted by this server) and scoped to one session.
    pub fn scoped(out: mpsc::UnboundedSender<Outbound>, session: SessionId) -> Self {
        Self {
            out,
            backlog: None,
            authed: true,
            subscription: None,
            scope: Some(session),
        }
    }

    /// Queue one outbound frame on this connection.
    pub fn enqueue(&self, outbound: Outbound) -> bool {
        enqueue_on(&self.out, self.backlog.as_deref(), outbound)
    }
}

/// Scope gate for a pinned connection (D5). Allowed: hello, ping, and
/// attach / interrupt / submit that name the pinned session. Approve is
/// checked per request in its own arm (it names no session). Every other
/// payload is refused by default, so a variant added later is denied
/// until someone decides otherwise here.
fn scoped_gate(scope: SessionId, payload: &Payload) -> Result<(), &'static str> {
    match payload {
        // Detach only drops this connection's own subscription.
        Payload::Hello { .. } | Payload::Ping {} | Payload::Approve { .. } | Payload::Detach {} => {
            Ok(())
        }
        Payload::Attach { session_id, .. }
        | Payload::Interrupt { session_id }
        | Payload::Steer { session_id, .. }
        | Payload::SteerAgent { session_id, .. }
        | Payload::KillAgent { session_id, .. }
        | Payload::Compact { session_id, .. }
        | Payload::Wake { session_id, .. }
        | Payload::CancelWake { session_id }
        | Payload::ExplainRule { session_id, .. }
        | Payload::Enqueue { session_id, .. }
        | Payload::Dequeue { session_id, .. }
        | Payload::DecidePlan { session_id, .. } => {
            if *session_id == scope {
                Ok(())
            } else {
                Err("this connection is scoped to another session")
            }
        }
        Payload::Submit { session_id, .. } => {
            // Submit may only continue the pinned session; a new session
            // (None) or a different id is refused.
            if *session_id == Some(scope) {
                Ok(())
            } else {
                Err("this connection can only submit to its own session")
            }
        }
        _ => Err("this request is not allowed on a share link"),
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
    error_kind(frame_id, message, modron_protocol::ErrorKind::Other)
}

/// `error` with a machine-readable kind (2026-11-12 audit: clients should
/// never have to match message strings).
#[allow(dead_code)]
fn error_kind(
    frame_id: Option<u64>,
    message: impl Into<String>,
    kind: modron_protocol::ErrorKind,
) -> Frame {
    reply(
        frame_id,
        None,
        Payload::Error {
            message: message.into(),
            kind: Some(kind),
        },
    )
}

/// Fire-and-forget send to one connection (a closed transport just drops
/// the frame).
pub(crate) fn send(ctx: &ConnCtx, frame: Frame) {
    let _ = ctx.enqueue(Outbound::Reply(Box::new(frame)));
}

/// Handle one client frame. Returns [`Flow::Close`] on auth failures.
pub async fn handle_frame(
    state: &Arc<ServerState>,
    ctx: &mut ConnCtx,
    trusted: bool,
    frame: Frame,
) -> Flow {
    let request_id = frame.id;
    if let Some(scope) = ctx.scope {
        if let Err(message) = scoped_gate(scope, &frame.payload) {
            send(
                ctx,
                error_kind(request_id, message, modron_protocol::ErrorKind::Forbidden),
            );
            return Flow::Continue;
        }
    }
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
                send(
                    ctx,
                    error_kind(request_id, message, modron_protocol::ErrorKind::Auth),
                );
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
            send(
                ctx,
                error_kind(
                    request_id,
                    "send a hello frame first",
                    modron_protocol::ErrorKind::Auth,
                ),
            );
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
                    .map(|meta| {
                        let task = state.tasks.get(&meta.id);
                        let task_state = task.as_ref().map(|_| {
                            if state
                                .hub
                                .handle(&meta.id)
                                .is_some_and(|handle| handle.is_busy())
                            {
                                "running".to_string()
                            } else {
                                "ready".to_string()
                            }
                        });
                        // The folder the session runs in: the hub handle
                        // holds the engine's resolved working dir (a
                        // worktree task runs in its worktree); a session
                        // with no live handle ran in the served repo or
                        // its worktree from an earlier run.
                        let working_dir = match state.hub.handle(&meta.id) {
                            Some(handle) => Some(handle.working_dir().display().to_string()),
                            None => task
                                .as_ref()
                                .map(|record| record.worktree.display().to_string())
                                .or_else(|| Some(state.working_dir.display().to_string())),
                        };
                        SessionMetaWire {
                            id: meta.id,
                            title: meta.title,
                            created_at: meta.created_at.to_rfc3339(),
                            updated_at: meta.updated_at.to_rfc3339(),
                            message_count: meta.message_count,
                            task_branch: task.as_ref().map(|r| r.branch.clone()),
                            task_state,
                            working_dir,
                        }
                    })
                    .collect();
                send(
                    ctx,
                    reply(request_id, None, Payload::Sessions { sessions: rows }),
                );
            }
            Err(e) => send(ctx, error(request_id, e.to_string())),
        },
        Payload::DeleteSession { session_id } => {
            handle_delete_session(state, ctx, request_id, session_id).await;
        }
        Payload::CreateTask { title, base } => {
            match crate::tasks::create_task(
                state,
                &state.hub,
                &state.working_dir,
                &title,
                base.as_deref(),
            )
            .await
            {
                Ok(record) => {
                    send(
                        ctx,
                        reply(
                            request_id,
                            Some(record.session_id),
                            Payload::TaskCreated {
                                session_id: record.session_id,
                                worktree: record.worktree.display().to_string(),
                                branch: record.branch.clone(),
                            },
                        ),
                    );
                }
                Err(e) => send(ctx, error(request_id, e.to_string())),
            }
        }
        Payload::RemoteControl {
            session_id,
            bridge_url,
        } => {
            // Empty bridge_url = use the server's --bridge config.
            let bridge = if bridge_url.is_empty() {
                state.bridge_base.clone()
            } else {
                Some(bridge_url.clone())
            };
            let Some(bridge) = bridge else {
                send(
                    ctx,
                    error(
                        request_id,
                        "no bridge configured - serve with --bridge <url> first",
                    ),
                );
                return Flow::Continue;
            };
            match crate::rc::start_remote_control(state, session_id, &state.working_dir, &bridge) {
                Ok(url) => send(
                    ctx,
                    reply(
                        request_id,
                        Some(session_id),
                        Payload::RemoteControlStarted { url },
                    ),
                ),
                Err(e) => send(ctx, error(request_id, e.to_string())),
            }
        }
        Payload::AutomationsList { .. } => {
            // 2026-11-11 audit: these three frames (plus publish_page)
            // fell through the catch-all - the desktop's automations UI
            // talked to a wall. CRUD answers with the refreshed list.
            let rows = crate::automations::list(state).await;
            send(ctx, reply(request_id, None, Payload::Automations { rows }));
        }
        Payload::AutomationAdd {
            name,
            prompt,
            interval_minutes,
        } => {
            let result = crate::automations::add(
                state,
                crate::automations::Automation {
                    name: name.clone(),
                    prompt,
                    interval_minutes,
                    last_run: String::new(),
                    next_due_epoch: None,
                },
            )
            .await;
            match result {
                Ok(()) => {
                    let rows = crate::automations::list(state).await;
                    send(ctx, reply(request_id, None, Payload::Automations { rows }));
                }
                Err(e) => send(ctx, error(request_id, e.to_string())),
            }
        }
        Payload::AutomationRemove { name } => {
            let _ = crate::automations::remove(state, &name).await;
            let rows = crate::automations::list(state).await;
            send(ctx, reply(request_id, None, Payload::Automations { rows }));
        }
        Payload::PublishPage {
            session_id,
            title,
            content_md,
        } => {
            handle_publish_page(state, ctx, request_id, session_id, title, content_md).await;
        }
        Payload::AuthStart {
            session_id,
            provider,
        } => {
            handle_auth_start(state, ctx, request_id, session_id, provider).await;
        }
        Payload::AuthSetKey {
            session_id,
            provider,
            key,
        } => {
            handle_auth_set_key(state, ctx, request_id, session_id, provider, key).await;
        }
        Payload::RouterDefaultsReq {} => {
            handle_router_defaults_req(state, ctx, request_id).await;
        }
        Payload::ModelsReq {} => {
            handle_models_req(state, ctx, request_id);
        }
        Payload::AuthProvidersReq {} | Payload::AuthProviders { .. } => {
            // AuthProvidersReq is the documented request shape; the
            // reply-shaped frame still works for older clients.
            let rows = auth_provider_rows();
            send(
                ctx,
                reply(request_id, None, Payload::AuthProviders { rows }),
            );
        }
        Payload::PersistRouter {
            cheap,
            strong,
            cheap_fallback,
            strong_fallback,
        } => {
            handle_persist_router(
                state,
                ctx,
                request_id,
                cheap,
                strong,
                cheap_fallback,
                strong_fallback,
            )
            .await;
        }
        Payload::GetTaskStatus { session_id } => {
            match crate::tasks::task_status(state, &session_id).await {
                Ok((record, ahead, files)) => {
                    let running = state
                        .hub
                        .handle(&session_id)
                        .is_some_and(|handle| handle.is_busy());
                    send(
                        ctx,
                        reply(
                            request_id,
                            Some(session_id),
                            Payload::TaskStatus {
                                session_id,
                                worktree: record.worktree.display().to_string(),
                                branch: record.branch.clone(),
                                running,
                                ahead,
                                files,
                            },
                        ),
                    );
                }
                Err(e) => send(ctx, error(request_id, e.to_string())),
            }
        }
        Payload::MergeTask {
            session_id,
            message,
        } => {
            match crate::tasks::merge_task(
                state,
                &state.working_dir,
                &session_id,
                message.as_deref(),
            )
            .await
            {
                Ok((record, ok, summary)) => {
                    send(
                        ctx,
                        reply(
                            request_id,
                            Some(session_id),
                            Payload::TaskMerged {
                                session_id,
                                ok,
                                summary,
                            },
                        ),
                    );
                    // The base checkout changed on success: refresh task
                    // state for attached clients.
                    let _ = record;
                }
                Err(e) => send(ctx, error(request_id, e.to_string())),
            }
        }
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
                    if handle.interrupt() {
                        // Success announces itself as TurnAborted. The aborted
                        // run never reached its own gauge refresh (H9b).
                        handle.refresh_context_detached();
                    } else {
                        send(ctx, error(request_id, "no active run in this session"));
                    }
                }
                None => send(
                    ctx,
                    error_kind(
                        request_id,
                        format!("no such session {session_id}"),
                        modron_protocol::ErrorKind::NotFound,
                    ),
                ),
            }
        }
        Payload::Steer { session_id, text } => {
            if text.trim().is_empty() {
                send(ctx, error(request_id, "steer text is empty"));
            } else if let Err(message) = state.hub.steer(session_id, text) {
                // An unknown session is NotFound; a known idle one is a
                // conflict with live state (submit instead).
                let kind = if state.hub.handle(&session_id).is_none() {
                    modron_protocol::ErrorKind::NotFound
                } else {
                    modron_protocol::ErrorKind::Conflict
                };
                send(ctx, error_kind(request_id, message, kind));
            }
        }
        // H3d: a sub-agent steer or stop. Errors go to the requester only;
        // success needs no reply (the row or the child's transcript shows it).
        Payload::SteerAgent {
            session_id,
            agent,
            text,
        } => {
            if text.trim().is_empty() {
                send(ctx, error(request_id, "steer text is empty"));
            } else if let Err(message) = state.hub.steer_agent(session_id, &agent, text) {
                send(
                    ctx,
                    error_kind(request_id, message, modron_protocol::ErrorKind::NotFound),
                );
            }
        }
        Payload::KillAgent { session_id, agent } => {
            if let Err(message) = state.hub.kill_agent(session_id, &agent) {
                send(
                    ctx,
                    error_kind(request_id, message, modron_protocol::ErrorKind::NotFound),
                );
            }
        }
        Payload::Compact { session_id } => {
            compact(state, ctx, request_id, session_id).await;
        }
        Payload::Wake {
            session_id,
            after_minutes,
            every_minutes,
            prompt,
        } => match crate::wake::validate(after_minutes, every_minutes, &prompt) {
            // Arguments are checked before anything is armed; the reply is
            // the WakeChanged frame the arm publishes.
            Err(message) => send(ctx, error(request_id, message)),
            Ok(schedule) => {
                let armed = crate::wake::set(
                    state,
                    session_id,
                    schedule,
                    prompt,
                    crate::wake::system_clock(),
                )
                .await;
                if let Err(message) = armed {
                    send(ctx, error(request_id, message));
                }
            }
        },
        Payload::CancelWake { session_id } => {
            if let Err(message) = crate::wake::cancel(state, session_id) {
                send(
                    ctx,
                    error_kind(request_id, message, modron_protocol::ErrorKind::NotFound),
                );
            }
        }
        Payload::Enqueue { session_id, text } => {
            if text.trim().is_empty() {
                send(ctx, error(request_id, "queue text is empty"));
            } else if state
                .hub
                .handle(&session_id)
                .is_some_and(|handle| handle.is_busy())
            {
                if let Err(message) = state.hub.enqueue(session_id, text) {
                    let kind = if state.hub.handle(&session_id).is_none() {
                        modron_protocol::ErrorKind::NotFound
                    } else {
                        modron_protocol::ErrorKind::Conflict
                    };
                    send(ctx, error_kind(request_id, message, kind));
                }
            } else {
                // Idle (or unknown) session: exactly the Submit path.
                submit(state, ctx, request_id, Some(session_id), text, Vec::new()).await;
            }
        }
        Payload::Dequeue { session_id, index } => {
            if let Err(message) = state.hub.dequeue(session_id, index) {
                let kind = if state.hub.handle(&session_id).is_none() {
                    modron_protocol::ErrorKind::NotFound
                } else {
                    modron_protocol::ErrorKind::Conflict
                };
                send(ctx, error_kind(request_id, message, kind));
            }
        }
        Payload::Approve {
            request_id: req,
            decision,
        } => {
            // A pinned connection resolves only inside its own session;
            // another session's request reads as unknown (never resolved).
            let outcome = match ctx.scope {
                Some(scope) => match state.hub.handle(&scope) {
                    Some(handle) => handle.resolve(&req, decision).map(|_| ()),
                    None => Err(format!("unknown or already-resolved request id {req}")),
                },
                None => state.hub.resolve_any(&req, decision).map(|_| ()),
            };
            match (outcome, ctx.scope) {
                (Ok(()), _) => {}
                // A pinned connection sees a foreign request as not found.
                (Err(message), Some(_)) => send(
                    ctx,
                    error_kind(request_id, message, modron_protocol::ErrorKind::NotFound),
                ),
                (Err(message), None) => send(ctx, error(request_id, message)),
            }
        }
        Payload::DecidePlan {
            session_id,
            decision,
            note,
        } => {
            decide_plan(state, ctx, request_id, session_id, decision, note).await;
        }
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
        Payload::ExplainRule {
            session_id,
            tool,
            args,
        } => match state.hub.handle(&session_id) {
            // Correlated reply: the rule lines against the rules the
            // session holds now (read-only, nothing is published).
            Some(handle) => {
                let text = handle.explain_rule(&tool, &args);
                send(
                    ctx,
                    reply(request_id, Some(session_id), Payload::Note { text }),
                );
            }
            None => send(
                ctx,
                error_kind(
                    request_id,
                    format!("no such session {session_id}"),
                    modron_protocol::ErrorKind::NotFound,
                ),
            ),
        },
        Payload::Fork { session_id } => {
            handle_fork(state, ctx, request_id, session_id).await;
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

/// `Compact` (H9b): the engine's compaction routine on an idle session. A
/// busy session is refused outright (the run owns the transcript). The
/// reply is the correlated Note; the gauge refresh (which publishes
/// `ContextChanged`) follows it, on success and failure alike.
async fn compact(
    state: &Arc<ServerState>,
    ctx: &mut ConnCtx,
    request_id: Option<u64>,
    session_id: SessionId,
) {
    let Some(handle) = state.hub.handle(&session_id) else {
        send(
            ctx,
            error_kind(
                request_id,
                format!("no such session {session_id}"),
                modron_protocol::ErrorKind::NotFound,
            ),
        );
        return;
    };
    if handle.is_busy() {
        send(
            ctx,
            error_kind(
                request_id,
                "session busy - compact when idle",
                modron_protocol::ErrorKind::Conflict,
            ),
        );
        return;
    }
    let engine = handle.engine_cell().clone();
    match engine.compact_session(session_id).await {
        Ok(outcome) => send(
            ctx,
            reply(
                request_id,
                Some(session_id),
                Payload::Note { text: outcome.note },
            ),
        ),
        Err(e) => send(ctx, error(request_id, e.to_string())),
    }
    handle.refresh_context().await;
}

/// The signed-in providers: the registry entries whose key or sign-in is
/// present. Shared by the router defaults and the model catalog.
fn signed_in_providers() -> Vec<String> {
    modron_core::auth::provider_registry()
        .iter()
        .filter(|p| modron_core::auth::catalog_provider_authenticated(p.provider_id))
        .map(|p| p.provider_id.to_string())
        .collect()
}

/// The model catalog rows for `models_req`, built from the same static
/// catalog the CLI routes with (`modron_router::catalog`).
fn handle_models_req(_state: &Arc<ServerState>, ctx: &mut ConnCtx, request_id: Option<u64>) {
    let models = models_for(modron_router::catalog(), |provider| {
        modron_core::auth::catalog_provider_authenticated(provider)
    });
    send(ctx, reply(request_id, None, Payload::Models { models }));
}

/// Catalog rows with `available` from `signed_in(provider)`, sorted
/// available first, then provider, then id. Takes the catalog and the
/// sign-in check as arguments so tests can inject both.
///
/// `available` asks the same per-provider check the router defaults use
/// (`catalog_provider_authenticated`) rather than the registry-derived
/// `authed` list: the catalog names providers that are not registry
/// entries (`chatgpt`, `ollama`), which that list can never contain.
fn models_for(
    catalog: &[modron_router::ModelInfo],
    signed_in: impl Fn(&str) -> bool,
) -> Vec<ModelWire> {
    let mut rows: Vec<ModelWire> = catalog
        .iter()
        .map(|info| ModelWire {
            id: info.name.clone(),
            provider: info.provider.clone(),
            label: info.name.clone(),
            context_window: Some(info.context_window),
            available: signed_in(&info.provider),
            tier: Some(info.tier.as_str().to_string()),
        })
        .collect();
    rows.sort_by(|a, b| {
        b.available
            .cmp(&a.available)
            .then_with(|| a.provider.cmp(&b.provider))
            .then_with(|| a.id.cmp(&b.id))
    });
    rows
}

async fn handle_router_defaults_req(
    _state: &Arc<ServerState>,
    ctx: &mut ConnCtx,
    request_id: Option<u64>,
) {
    // What the settings page's defaults card prefills: the authed
    // catalog providers, the derived recommendation (the same
    // auto-routing the serve startup notes), and the fallback
    // chains that follow.
    let authed: Vec<String> = signed_in_providers();
    let (recommended_cheap, recommended_strong, cheap_fallback, strong_fallback) =
        match modron_router::derive_tier_candidates(&authed) {
            Some(candidates) => (
                candidates.cheap.first().cloned(),
                candidates.strong.first().cloned(),
                candidates.cheap.iter().skip(1).cloned().collect(),
                candidates.strong.iter().skip(1).cloned().collect(),
            ),
            None => match modron_router::derive_tiers(&authed) {
                Some((cheap, strong)) => (Some(cheap), Some(strong), Vec::new(), Vec::new()),
                None => (None, None, Vec::new(), Vec::new()),
            },
        };
    send(
        ctx,
        reply(
            request_id,
            None,
            Payload::RouterDefaults {
                authed,
                recommended_cheap,
                recommended_strong,
                cheap_fallback,
                strong_fallback,
            },
        ),
    );
}

async fn handle_fork(
    state: &Arc<ServerState>,
    ctx: &mut ConnCtx,
    request_id: Option<u64>,
    session_id: SessionId,
) {
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

/// The hub handle for a session that already exists in the store, created
/// on demand. A worktree task's session runs its engine INSIDE the task's
/// worktree: after a server restart the handle is gone, and re-creating it
/// without the recorded worktree would run the task in the base checkout.
pub(crate) fn ensure_session_for(
    state: &Arc<ServerState>,
    id: SessionId,
) -> Arc<crate::hub::SessionHandle> {
    // Every run (Submit, or a queued follow-up) reaches the
    // hub through here, so the queue starter is installed before any run
    // can complete. Weak: the hub must not keep the server state alive.
    if !state.hub.has_run_starter() {
        state
            .hub
            .set_run_starter(queued_run_starter(Arc::downgrade(state)));
    }
    let worktree = state.tasks.get(&id).map(|record| record.worktree);
    state.hub.ensure_session_in(id, Some(id), worktree)
}

/// Answer a proposed plan (H2b). `Approve` flips the session to `build`
/// (echoed as ConfigChanged), records PlanDecided, and starts a run that
/// executes the plan. `Revise` keeps `plan` phase, records PlanDecided, and
/// starts another plan run with the note as the task. Both use the resume
/// path a Submit to a known session uses.
async fn decide_plan(
    state: &Arc<ServerState>,
    ctx: &mut ConnCtx,
    request_id: Option<u64>,
    session_id: SessionId,
    decision: PlanDecision,
    note: Option<String>,
) {
    let task = match plan_decision_task(decision, note) {
        Ok(task) => task,
        Err(message) => {
            send(ctx, error(request_id, message));
            return;
        }
    };
    let Some(handle) = state.hub.handle(&session_id) else {
        send(
            ctx,
            error_kind(
                request_id,
                format!("no such session {session_id}"),
                modron_protocol::ErrorKind::NotFound,
            ),
        );
        return;
    };
    if !handle.plan_pending() {
        send(ctx, error(request_id, NO_PLAN_PENDING));
        return;
    }
    if handle.is_busy() {
        send(
            ctx,
            error_kind(
                request_id,
                "session busy (a run is active)",
                modron_protocol::ErrorKind::Conflict,
            ),
        );
        return;
    }
    if decision == PlanDecision::Approve {
        let patch = ConfigPatch {
            phase: Some(Phase::Build),
            ..ConfigPatch::default()
        };
        if let Err(e) = handle.apply_config(&patch) {
            send(ctx, error(request_id, e.to_string()));
            return;
        }
        handle.publish(Payload::ConfigChanged { patch });
    }
    if let Err(message) = handle.decide_plan(decision) {
        send(ctx, error(request_id, message));
        return;
    }
    let run = resume_run(&handle, task, Vec::new(), true);
    if let Err(message) = state.hub.start_run(session_id, run) {
        send(ctx, error(request_id, message));
    }
}

/// The task a plan decision runs (H2b). Approve executes the approved plan,
/// with the note on its own line when present. Revise plans again with the
/// note as the task, so a note is required.
fn plan_decision_task(
    decision: PlanDecision,
    note: Option<String>,
) -> Result<String, &'static str> {
    let note = note.filter(|text| !text.trim().is_empty());
    match (decision, note) {
        (PlanDecision::Approve, Some(note)) => Ok(format!("Execute the approved plan.\n{note}")),
        (PlanDecision::Approve, None) => Ok("Execute the approved plan.".to_string()),
        (PlanDecision::Revise, Some(note)) => Ok(note),
        (PlanDecision::Revise, None) => Err("revising a plan needs a note"),
    }
}

/// The starter the hub calls when a queued follow-up must run: the same
/// run construction as a Submit to a known session. A failed start is
/// reported as a Note; the remaining queue stays as it is.
fn queued_run_starter(state: std::sync::Weak<ServerState>) -> RunStarter {
    Arc::new(move |id: SessionId, text: String| {
        let Some(state) = state.upgrade() else {
            return;
        };
        let Some(handle) = state.hub.handle(&id) else {
            return;
        };
        // A queued message exists only after a run completed, and that run
        // persisted its task, so the session has events to resume.
        let run = resume_run(&handle, text, Vec::new(), true);
        if let Err(message) = state.hub.start_run(id, run) {
            handle.publish(Payload::Note {
                text: format!("queued message not started: {message}"),
            });
        }
    })
}

/// Build the run that continues a known session with `task`. Re-arms the
/// adoption pin, then wraps the task with the standing goal, exactly as a
/// Submit to a known session always has. `has_events` picks resume (the
/// log has events) over a fresh run in an empty `new_session` shell.
fn resume_run(
    handle: &crate::hub::SessionHandle,
    task: String,
    attachments: Vec<modron_core::Attachment>,
    has_events: bool,
) -> RunFuture {
    // Re-arm the adoption pin: a known session always runs in itself, even
    // when an earlier run already consumed the pin without writing an event
    // (LG-e).
    handle.rearm_adoption();
    let id = handle.id;
    let engine = handle.engine_cell().clone();
    let goal = handle_goal(handle);
    let full_task = task_with_goal(&goal, &task);
    if has_events {
        Box::pin(async move {
            engine
                .run_task_resuming_with_attachments(id, &full_task, attachments)
                .await
        })
    } else {
        Box::pin(async move {
            engine
                .run_task_with_attachments(&full_task, attachments)
                .await
        })
    }
}

async fn handle_delete_session(
    state: &Arc<ServerState>,
    ctx: &mut ConnCtx,
    request_id: Option<u64>,
    session_id: SessionId,
) {
    // A running session keeps its history until it settles. The busy check
    // and the fence against new runs are one step (LG-b): no run can start
    // between them and the removal below.
    if let Err(message) = state.hub.begin_delete(session_id) {
        send(ctx, error(request_id, message));
        return;
    }
    match state.store.delete(session_id).await {
        Ok(removed) => {
            state.hub.finish_delete(session_id);
            forget_session_state(state, session_id);
            if removed {
                send(
                    ctx,
                    reply(
                        request_id,
                        Some(session_id),
                        Payload::Note {
                            text: format!("session deleted: {session_id}"),
                        },
                    ),
                );
            } else {
                send(
                    ctx,
                    error_kind(
                        request_id,
                        format!("no such session {session_id}"),
                        modron_protocol::ErrorKind::NotFound,
                    ),
                );
            }
        }
        Err(e) => {
            state.hub.cancel_delete(session_id);
            send(ctx, error(request_id, format!("delete failed: {e}")));
        }
    }
}

/// Drop the server-side state a deleted session leaves behind (LG-b): its
/// share key, its live remote-control tunnel (aborted, not just forgotten)
/// and its worktree-task record. The worktree directory itself is left on
/// disk.
fn forget_session_state(state: &Arc<ServerState>, session_id: SessionId) {
    fn guard<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
    guard(&state.rc_keys).remove(&session_id);
    if let Some(tunnel) = guard(&state.rc_tunnels).remove(&session_id) {
        tunnel.abort();
    }
    state.tasks.remove(&session_id);
    crate::wake::forget(state, session_id);
}

async fn handle_publish_page(
    state: &Arc<ServerState>,
    ctx: &mut ConnCtx,
    request_id: Option<u64>,
    session_id: SessionId,
    title: String,
    content_md: String,
) {
    let Some(bridge) = state.bridge_base.clone() else {
        send(
            ctx,
            error(
                request_id,
                "no bridge configured - serve with --bridge <url> first",
            ),
        );
        return;
    };
    let key = match crate::rc::share_key_for(state, &session_id) {
        Some(key) => key,
        None => {
            // Mint a share key the same way remote-control does;
            // the bridge authenticates agent posts by it.
            let key = crate::rc::share_key();
            state
                .rc_keys
                .lock()
                .unwrap()
                .insert(session_id, key.clone());
            key
        }
    };
    match crate::rc::publish_page(&bridge, &key, session_id, &title, &content_md).await {
        Ok(url) => send(
            ctx,
            reply(request_id, Some(session_id), Payload::PagePublished { url }),
        ),
        Err(e) => send(ctx, error(request_id, e.to_string())),
    }
}

async fn handle_auth_start(
    state: &Arc<ServerState>,
    ctx: &mut ConnCtx,
    request_id: Option<u64>,
    session_id: SessionId,
    provider: String,
) {
    // The provider's sign-in flow runs HERE (the browser opens on
    // the server machine); progress streams to the session as
    // oauth_progress frames and the outcome lands as a note.
    if !modron_core::auth::oauth_capable(&provider) {
        send(
            ctx,
            error(
                request_id,
                format!("{provider} uses an api key - set it below"),
            ),
        );
        return;
    }
    let Some(handle) = state.hub.handle(&session_id) else {
        send(ctx, error(request_id, "attach a session first"));
        return;
    };
    let publish = move |payload: Payload| handle.publish(payload);
    let provider_kind = provider.clone();
    tokio::spawn(async move {
        let progress =
            |stage: &str, message: &str, url: Option<String>, user_code: Option<String>| {
                publish(Payload::OAuthProgress {
                    provider: provider_kind.clone(),
                    stage: stage.to_string(),
                    message: message.to_string(),
                    url,
                    user_code,
                });
            };
        let result: anyhow::Result<String> = async {
            match provider_kind.as_str() {
                "anthropic" => {
                    let session = modron_core::auth::oauth::prepare_login()?;
                    progress(
                        "waiting",
                        "approve in the browser window",
                        Some(session.url.clone()),
                        None,
                    );
                    let code = modron_core::auth::oauth::wait_for_callback(
                        session.port,
                        &session.state,
                        Some(&session.url),
                    )
                    .await?;
                    let tokens = modron_core::auth::oauth::exchange_code(&session, &code).await?;
                    modron_core::auth::oauth::save_tokens(&tokens)?;
                    Ok(format!(
                        "claude signed in ({})",
                        modron_core::auth::mask_key(&tokens.access_token)
                    ))
                }
                "zai" => {
                    let session = modron_core::auth::oauth::zai::prepare_login()?;
                    progress(
                        "waiting",
                        "approve in the browser window",
                        Some(session.url.clone()),
                        None,
                    );
                    let code = modron_core::auth::oauth::wait_for_callback(
                        session.port,
                        &session.state,
                        Some(&session.url),
                    )
                    .await?;
                    let (key, email) =
                        modron_core::auth::oauth::zai::finish(&code, &session.state, session.port)
                            .await?;
                    let path = modron_core::auth::env_file()
                        .map_err(|e| anyhow::anyhow!("cannot resolve env file: {e}"))?;
                    modron_core::auth::set_key_in_env_file(&path, "ZAI_API_KEY", &key)
                        .map_err(|e| anyhow::anyhow!("cannot save key: {e}"))?;
                    std::env::set_var("ZAI_API_KEY", &key);
                    Ok(format!(
                        "z.ai signed in{} - GLM coding plan key saved",
                        email.map(|e| format!(" ({e})")).unwrap_or_default()
                    ))
                }
                _ => {
                    // The ChatGPT device flow (openai-compat).
                    let device = modron_core::auth::oauth::chatgpt::start().await?;
                    modron_core::auth::open_browser(&device.verification_uri);
                    progress(
                        "waiting",
                        "enter the code in the browser window",
                        Some(device.verification_uri.clone()),
                        Some(device.user_code.clone()),
                    );
                    let (tokens, account) =
                        modron_core::auth::oauth::chatgpt::finish(&device).await?;
                    modron_core::auth::oauth::chatgpt::save(
                        &modron_core::auth::oauth::chatgpt::ChatGptAuth {
                            access_token: tokens.access_token.clone(),
                            refresh_token: tokens.refresh_token.clone(),
                            account_id: account,
                        },
                    )?;
                    Ok(format!(
                        "chatgpt signed in ({})",
                        modron_core::auth::mask_key(&tokens.access_token)
                    ))
                }
            }
        }
        .await;
        match result {
            Ok(text) => {
                progress("done", &text, None, None);
                publish(Payload::Note {
                    text: format!("{provider_kind}: {text}"),
                });
            }
            Err(e) => {
                progress("failed", &e.to_string(), None, None);
                publish(Payload::Note {
                    text: format!("{provider_kind} sign-in failed: {e}"),
                });
            }
        }
        // Providers statuses changed; the settings page re-reads
        // them on the next AuthProviders round trip. Nudge by
        // noting completion (clients refresh on the done stage).
    });
    send(
        ctx,
        reply(
            request_id,
            Some(session_id),
            Payload::Note {
                text: format!("{provider}: sign-in started - watch the session for the code"),
            },
        ),
    );
}

async fn handle_auth_set_key(
    _state: &Arc<ServerState>,
    ctx: &mut ConnCtx,
    request_id: Option<u64>,
    session_id: SessionId,
    provider: String,
    key: String,
) {
    let info = modron_core::auth::provider_registry()
        .into_iter()
        .find(|p| p.provider_id == provider);
    let Some(info) = info else {
        send(
            ctx,
            error_kind(
                request_id,
                format!("unknown provider {provider}"),
                modron_protocol::ErrorKind::NotFound,
            ),
        );
        return;
    };
    let path = match modron_core::auth::env_file() {
        Ok(path) => path,
        Err(e) => {
            send(
                ctx,
                error(request_id, format!("cannot resolve env file: {e}")),
            );
            return;
        }
    };
    match modron_core::auth::set_key_in_env_file(&path, info.env_var, &key) {
        Ok(()) => {
            std::env::set_var(info.env_var, &key);
            let _ = session_id;
            send(
                ctx,
                reply(
                    request_id,
                    None,
                    Payload::Note {
                        text: format!("{}: api key saved", info.name),
                    },
                ),
            );
        }
        Err(e) => send(ctx, error(request_id, format!("cannot save key: {e}"))),
    }
}

async fn handle_persist_router(
    state: &Arc<ServerState>,
    ctx: &mut ConnCtx,
    request_id: Option<u64>,
    cheap: String,
    strong: String,
    cheap_fallback: Option<String>,
    strong_fallback: Option<String>,
) {
    let cheap_provider = modron_router::lookup(&cheap)
        .map(|i| i.provider.clone())
        .unwrap_or_default();
    let strong_provider = modron_router::lookup(&strong)
        .map(|i| i.provider.clone())
        .unwrap_or_default();
    let result = (|| -> anyhow::Result<()> {
        crate::tasks::persist_router_toml(
            &state.working_dir,
            &cheap,
            &strong,
            cheap_fallback.as_deref(),
            strong_fallback.as_deref(),
        )?;
        state.hub.apply_router_defaults(
            &cheap,
            &cheap_provider,
            &strong,
            &strong_provider,
            cheap_fallback.as_deref(),
            strong_fallback.as_deref(),
        );
        Ok(())
    })();
    match result {
        Ok(()) => send(
            ctx,
            reply(
                request_id,
                None,
                Payload::Note {
                    text: format!(
                        "router defaults saved: cheap {cheap} ({}), strong {strong} ({})",
                        if cheap_provider.is_empty() {
                            "?"
                        } else {
                            &cheap_provider
                        },
                        if strong_provider.is_empty() {
                            "?"
                        } else {
                            &strong_provider
                        }
                    ),
                },
            ),
        ),
        Err(e) => send(ctx, error(request_id, e.to_string())),
    }
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
            let handle = ensure_session_for(state, meta.id);
            let engine = handle.engine_cell().clone();
            let run =
                Box::pin(async move { engine.run_task_with_attachments(&task, attachments).await });
            (handle, run)
        }
        Some(id) => {
            // Known session: resume when the log has events; adopt (pinned
            // run) when it is an empty shell from `new_session`. An unknown
            // id is an error, never a silent new session. event_count, not
            // load: this runs per submit, and the engine re-loads the log
            // itself for the resume - parsing it here too was O(log) twice
            // (cycle-2 audit H6).
            let event_count = match state.store.event_count(id).await {
                Ok(count) => count,
                Err(e) => {
                    send(
                        ctx,
                        error_kind(
                            request_id,
                            format!("no such session {id}: {e}"),
                            modron_protocol::ErrorKind::NotFound,
                        ),
                    );
                    return;
                }
            };
            let handle = ensure_session_for(state, id);
            let run = resume_run(&handle, task, attachments, event_count > 0);
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
            // would happily fabricate a handle. event_count: cheap probe,
            // no full parse (cycle-2 audit H6).
            match state.store.event_count(session_id).await {
                Ok(_) => {}
                Err(e) => {
                    send(
                        ctx,
                        error(request_id, format!("no such session {session_id}: {e}")),
                    );
                    return;
                }
            }
            ensure_session_for(state, session_id)
        }
    };

    // 1. Subscribe BEFORE loading anything, buffering live frames.
    let rx = handle.subscribe();
    let (buf_tx, mut buf_rx) = mpsc::unbounded_channel::<Outbound>();
    let forward = tokio::spawn(async move {
        let mut rx = rx;
        loop {
            match rx.recv().await {
                Ok(live) => {
                    if buf_tx.send(Outbound::Live(Box::new(live))).is_err() {
                        break;
                    }
                }
                // A lagged receiver silently skipped frames forever -
                // deltas would garble and events left holes with no
                // client-visible signal (cycle-2 audit M8). End the
                // subscription and tell the client to re-attach (its
                // re-attach replays from the authoritative store).
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "client lagged the session stream - detaching");
                    let _ = buf_tx.send(Outbound::Reply(Box::new(Frame::event(
                        None,
                        0,
                        Payload::Error {
                            message: "live stream lagged - re-attach to resync".into(),
                            kind: Some(modron_protocol::ErrorKind::Conflict),
                        },
                    ))));
                    break;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // 2. Load the persisted log + live state; send the snapshot first.
    // The load holds the per-session APPEND lock: no append can write to
    // disk or publish while it is held, so the watermark read afterwards
    // is EXACT - frames at or below it are already inside the snapshot,
    // frames above it are not (the cycle-2 M1 fix's watermark-after-load
    // had a loss window: an append completing between load and the read
    // was in NEITHER; cycle-3 F1 closed it by holding the append lock
    // across both).
    let (append_guard, events) = match state.store.load_snapshot_locked(session_id).await {
        Ok(loaded) => loaded,
        Err(e) => {
            forward.abort();
            send(ctx, error(request_id, e.to_string()));
            return;
        }
    };
    // The watermark and the live state come from ONE read under the seq
    // lock (LG-d). Every sink mutates its buffers and publishes under that
    // lock, so the live state holds exactly the frames at or below the
    // watermark; the append lock still held makes the persisted log match it.
    let (watermark, live) = handle.snapshot_point();
    drop(append_guard);
    let snapshot = Frame {
        id: request_id,
        session: Some(session_id),
        seq: watermark,
        payload: Payload::Snapshot {
            session_id,
            last_seq: watermark,
            events,
            live,
        },
    };
    if !ctx.enqueue(Outbound::Reply(Box::new(snapshot))) {
        forward.abort();
        return;
    }

    // 3. Forward the buffered live frames strictly after the watermark:
    // frames at or below it are already inside the snapshot (cycle-2 audit
    // M1 - the doc claimed clients deduped by seq; none ever did).
    let out = ctx.out.clone();
    let backlog = ctx.backlog.clone();
    let pump = tokio::spawn(async move {
        while let Some(outbound) = buf_rx.recv().await {
            if let Outbound::Live(live) = &outbound {
                if live.seq <= watermark {
                    continue;
                }
            }
            if !enqueue_on(&out, backlog.as_deref(), outbound) {
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

/// Provider auth status rows (the AuthProvidersReq answer).
fn auth_provider_rows() -> Vec<modron_protocol::AuthProviderWire> {
    use modron_core::auth::{oauth_capable, oauth_signed_in, provider_registry};
    provider_registry()
        .iter()
        .map(|info| {
            let status = if oauth_signed_in(info.provider_id) {
                "subscription (oauth)".to_string()
            } else if std::env::var(info.env_var).is_ok() {
                "api key".to_string()
            } else {
                "not configured".to_string()
            };
            modron_protocol::AuthProviderWire {
                id: info.provider_id.to_string(),
                label: info.name.to_string(),
                status,
                oauth_capable: oauth_capable(info.provider_id),
            }
        })
        .collect()
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
        Payload::Steer { .. } => "steer",
        Payload::SteerAgent { .. } => "steer_agent",
        Payload::KillAgent { .. } => "kill_agent",
        Payload::AgentFinished { .. } => "agent_finished",
        Payload::Steered { .. } => "steered",
        Payload::Enqueue { .. } => "enqueue",
        Payload::Dequeue { .. } => "dequeue",
        Payload::Compact { .. } => "compact",
        Payload::QueueChanged { .. } => "queue_changed",
        Payload::ContextChanged { .. } => "context_changed",
        Payload::Wake { .. } => "wake",
        Payload::CancelWake { .. } => "cancel_wake",
        Payload::WakeChanged { .. } => "wake_changed",
        Payload::Notify { .. } => "notify",
        Payload::Approve { .. } => "approve",
        Payload::DecidePlan { .. } => "decide_plan",
        Payload::PlanProposed { .. } => "plan_proposed",
        Payload::PlanDecided { .. } => "plan_decided",
        Payload::SetConfig { .. } => "set_config",
        Payload::ExplainRule { .. } => "explain_rule",
        Payload::Fork { .. } => "fork",
        Payload::NewSession { .. } => "new_session",
        Payload::ListSessions {} => "list_sessions",
        Payload::DeleteSession { .. } => "delete_session",
        Payload::AuthStart { .. } => "auth_start",
        Payload::RouterDefaultsReq { .. } => "router_defaults_req",
        Payload::RouterDefaults { .. } => "router_defaults",
        Payload::ModelsReq {} => "models_req",
        Payload::Models { .. } => "models",
        Payload::AuthSetKey { .. } => "auth_set_key",
        Payload::AuthProviders { .. } | Payload::AuthProvidersReq {} => "auth_providers",
        Payload::PersistRouter { .. } => "persist_router",
        Payload::CreateTask { .. } => "create_task",
        Payload::GetTaskStatus { .. } => "get_task_status",
        Payload::RemoteControl { .. } => "remote_control",
        Payload::RemoteControlStarted { .. } => "remote_control_started",
        Payload::AutomationsList { .. } => "automations_list",
        Payload::AutomationAdd { .. } => "automation_add",
        Payload::AutomationRemove { .. } => "automation_remove",
        Payload::Automations { .. } => "automations",
        Payload::PublishPage { .. } => "publish_page",
        Payload::PagePublished { .. } => "page_published",
        Payload::MergeTask { .. } => "merge_task",
        Payload::TaskCreated { .. } => "task_created",
        Payload::TaskStatus { .. } => "task_status",
        Payload::TaskMerged { .. } => "task_merged",
        Payload::Ping {} => "ping",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::{BroadcastingStore, LiveFrame};
    use crate::tasks::TaskRecord;
    use modron_core::testing::StaticSelector;
    use modron_core::{Engine, EngineConfig, RunSummary, SessionStore, ToolRegistry};
    use std::time::Duration;

    /// A served state over an on-disk store (delete needs a real store),
    /// wired like the real server: the engine's store is the hub's
    /// broadcasting store. The TempDir must outlive the state.
    fn test_state() -> (Arc<ServerState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let hub = SessionHub::empty();
        let inner: Arc<dyn SessionStore> = Arc::new(modron_store::JsonlSessionStore::new(
            dir.path().join("sessions"),
        ));
        let store: Arc<dyn SessionStore> = Arc::new(BroadcastingStore::new(inner, hub.clone()));
        hub.set_engine(Engine {
            selector: Arc::new(StaticSelector::new("mock-model", "mock")),
            providers: Default::default(),
            tools: Arc::new(ToolRegistry::new()),
            store: Arc::clone(&store),
            config: EngineConfig {
                default_model: "mock-model".into(),
                default_provider: "mock".into(),
                ..EngineConfig::default()
            },
        });
        let state = Arc::new(ServerState {
            hub,
            store,
            devices_dir: dir.path().to_path_buf(),
            version: "test".into(),
            working_dir: dir.path().to_path_buf(),
            tasks: crate::tasks::TaskRegistry::new(),
            bridge_base: None,
            rc_keys: std::sync::Mutex::new(HashMap::new()),
            rc_tunnels: std::sync::Mutex::new(HashMap::new()),
        });
        (state, dir)
    }

    /// An authenticated connection and its outbound receiver.
    fn authed_ctx() -> (ConnCtx, mpsc::UnboundedReceiver<Outbound>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut ctx = ConnCtx::new(tx);
        ctx.authed = true;
        (ctx, rx)
    }

    async fn next_outbound(rx: &mut mpsc::UnboundedReceiver<Outbound>) -> Outbound {
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("an outbound frame")
            .expect("channel open")
    }

    fn parked_run(id: SessionId) -> RunFuture {
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
        })
    }

    /// H5a: a `Wake` with bad arguments or a blank prompt is refused with an
    /// error and arms nothing; a good one arms, and `CancelWake` clears it.
    #[tokio::test]
    async fn wake_frames_validate_arguments_then_arm_and_cancel() {
        let (state, _dir) = test_state();
        let id = state.store.create_session("wake").await.unwrap().id;
        state
            .store
            .append(
                id,
                &modron_core::SessionEvent::Task {
                    text: "earlier".into(),
                },
            )
            .await
            .unwrap();
        let (mut ctx, mut rx) = authed_ctx();
        let bad = [
            (Some(5), Some(5), "x"),
            (None, None, "x"),
            (None, Some(4), "x"),
            (Some(0), None, "x"),
            (Some(5), None, "   "),
        ];
        for (after, every, prompt) in bad {
            handle_frame(
                &state,
                &mut ctx,
                true,
                Frame::request(
                    1,
                    Payload::Wake {
                        session_id: id,
                        after_minutes: after,
                        every_minutes: every,
                        prompt: prompt.into(),
                    },
                ),
            )
            .await;
            assert!(!error_message(next_outbound(&mut rx).await).is_empty());
        }
        assert!(state
            .hub
            .handle(&id)
            .is_none_or(|handle| handle.live_state().wake.is_none()));

        handle_frame(
            &state,
            &mut ctx,
            true,
            Frame::request(
                2,
                Payload::Wake {
                    session_id: id,
                    after_minutes: Some(30),
                    every_minutes: None,
                    prompt: "check later".into(),
                },
            ),
        )
        .await;
        let handle = state.hub.handle(&id).expect("armed session is live");
        assert_eq!(
            handle.live_state().wake.map(|wake| wake.prompt),
            Some("check later".to_string())
        );

        handle_frame(
            &state,
            &mut ctx,
            true,
            Frame::request(3, Payload::CancelWake { session_id: id }),
        )
        .await;
        assert!(handle.live_state().wake.is_none());
        handle_frame(
            &state,
            &mut ctx,
            true,
            Frame::request(4, Payload::CancelWake { session_id: id }),
        )
        .await;
        assert!(error_message(next_outbound(&mut rx).await).contains("no wake"));
    }

    /// H9b: `Compact` on a busy session is refused with a conflict error; the
    /// active run keeps the transcript.
    #[tokio::test]
    async fn compact_on_a_busy_session_errors() {
        let (state, _dir) = test_state();
        let id = state.store.create_session("busy").await.unwrap().id;
        state.hub.ensure_session(id, None);
        state.hub.start_run(id, parked_run(id)).unwrap();

        let (mut ctx, mut rx) = authed_ctx();
        handle_frame(
            &state,
            &mut ctx,
            true,
            Frame::request(1, Payload::Compact { session_id: id }),
        )
        .await;
        match next_outbound(&mut rx).await {
            Outbound::Reply(frame) => match frame.payload {
                Payload::Error { message, .. } => {
                    assert!(message.contains("compact when idle"), "{message}");
                }
                other => panic!("expected an error, got {other:?}"),
            },
            other => panic!("expected a reply, got {other:?}"),
        }
        state.hub.handle(&id).unwrap().interrupt();
    }

    /// H3d: a sub-agent steer or stop that cannot land is refused to the
    /// requester: an unknown agent, or an empty steer.
    #[tokio::test]
    async fn steer_and_kill_agent_refuse_unknown_agents_and_empty_steers() {
        let (state, _dir) = test_state();
        let id = state.store.create_session("agents").await.unwrap().id;
        state.hub.ensure_session(id, None);
        let (mut ctx, mut rx) = authed_ctx();
        let requests = [
            Payload::KillAgent {
                session_id: id,
                agent: "nobody".into(),
            },
            Payload::SteerAgent {
                session_id: id,
                agent: "nobody".into(),
                text: "x".into(),
            },
            Payload::SteerAgent {
                session_id: id,
                agent: "nobody".into(),
                text: "   ".into(),
            },
        ];
        let expected = [
            "no live sub-agent",
            "no live sub-agent",
            "steer text is empty",
        ];
        for (n, payload) in requests.into_iter().enumerate() {
            handle_frame(
                &state,
                &mut ctx,
                true,
                Frame::request(n as u64 + 1, payload),
            )
            .await;
            match next_outbound(&mut rx).await {
                Outbound::Reply(frame) => match frame.payload {
                    Payload::Error { message, .. } => {
                        assert!(message.contains(expected[n]), "{message}");
                    }
                    other => panic!("expected an error, got {other:?}"),
                },
                other => panic!("expected a reply, got {other:?}"),
            }
        }
    }

    /// H9b: `Compact` on an idle session replies with a correlated Note, then
    /// publishes the refreshed gauge; LiveState carries it from then on.
    #[tokio::test]
    async fn compact_on_an_idle_session_replies_with_a_note_and_publishes_context() {
        let (state, _dir) = test_state();
        let id = state.store.create_session("idle").await.unwrap().id;
        let handle = state.hub.ensure_session(id, None);
        let mut live = handle.subscribe();

        let (mut ctx, mut rx) = authed_ctx();
        handle_frame(
            &state,
            &mut ctx,
            true,
            Frame::request(2, Payload::Compact { session_id: id }),
        )
        .await;
        match next_outbound(&mut rx).await {
            Outbound::Reply(frame) => match frame.payload {
                Payload::Note { text } => {
                    assert!(text.contains("nothing to compact"), "{text}");
                }
                other => panic!("expected a note, got {other:?}"),
            },
            other => panic!("expected a reply, got {other:?}"),
        }

        let frame = tokio::time::timeout(Duration::from_secs(2), live.recv())
            .await
            .expect("a live frame")
            .expect("channel open");
        let json: serde_json::Value = serde_json::from_str(&frame.json).unwrap();
        assert_eq!(json["type"], "context_changed");
        assert!(json["used"].as_u64().unwrap() > 0, "system prompt counts");
        let context = handle.live_state().context.expect("gauge in live state");
        assert_eq!(context.used, json["used"].as_u64().unwrap());
        assert_eq!(context.window, json["window"].as_u64().unwrap());
    }

    /// LG-b: deleting a session removes its share key, its live tunnel and
    /// its task record, not just the transcript and the live handle.
    #[tokio::test]
    async fn delete_session_forgets_its_rc_key_tunnel_and_task_record() {
        let (state, _dir) = test_state();
        let id = state.store.create_session("doomed").await.unwrap().id;
        state.hub.ensure_session(id, None);
        state.rc_keys.lock().unwrap().insert(id, "key".into());
        let tunnel = tokio::spawn(std::future::pending::<()>());
        state
            .rc_tunnels
            .lock()
            .unwrap()
            .insert(id, tunnel.abort_handle());
        state.tasks.insert(TaskRecord {
            session_id: id,
            worktree: std::env::temp_dir(),
            branch: "task-branch".into(),
            base: "main".into(),
            title: "doomed".into(),
        });

        let (mut ctx, mut rx) = authed_ctx();
        handle_frame(
            &state,
            &mut ctx,
            true,
            Frame::request(1, Payload::DeleteSession { session_id: id }),
        )
        .await;
        let reply = next_outbound(&mut rx).await;
        assert!(
            matches!(reply, Outbound::Reply(ref f) if matches!(f.payload, Payload::Note { .. })),
            "delete succeeds: {reply:?}"
        );

        assert!(state.rc_keys.lock().unwrap().get(&id).is_none());
        assert!(state.rc_tunnels.lock().unwrap().get(&id).is_none());
        assert!(
            tunnel.await.unwrap_err().is_cancelled(),
            "the live tunnel task is aborted"
        );
        assert!(state.tasks.get(&id).is_none());
        assert!(state.hub.handle(&id).is_none());
        assert!(state
            .store
            .list()
            .await
            .unwrap()
            .iter()
            .all(|meta| meta.id != id));
    }

    /// LG-b: a running session is refused, and nothing is removed.
    /// After a restart the hub has no handle for a task session; the one
    /// re-created on attach/submit must run in the task's worktree, not in
    /// the served repo.
    #[tokio::test]
    async fn a_recreated_task_session_runs_in_its_worktree() {
        let (state, _dir) = test_state();
        let worktree = tempfile::tempdir().unwrap();
        let task = state.store.create_session("task").await.unwrap().id;
        let plain = state.store.create_session("plain").await.unwrap().id;
        state.tasks.insert(TaskRecord {
            session_id: task,
            worktree: worktree.path().to_path_buf(),
            branch: "task-branch".into(),
            base: "main".into(),
            title: "task".into(),
        });
        assert!(state.hub.handle(&task).is_none(), "no live handle yet");
        let handle = ensure_session_for(&state, task);
        assert_eq!(handle.working_dir(), worktree.path());
        let handle = ensure_session_for(&state, plain);
        assert_ne!(
            handle.working_dir(),
            worktree.path(),
            "a non-task session keeps the engine's own working dir"
        );
    }

    #[tokio::test]
    async fn delete_session_refuses_a_running_session_and_keeps_its_state() {
        let (state, _dir) = test_state();
        let id = state.store.create_session("busy").await.unwrap().id;
        state.hub.ensure_session(id, None);
        state.hub.start_run(id, parked_run(id)).unwrap();
        state.rc_keys.lock().unwrap().insert(id, "key".into());

        let (mut ctx, mut rx) = authed_ctx();
        handle_frame(
            &state,
            &mut ctx,
            true,
            Frame::request(1, Payload::DeleteSession { session_id: id }),
        )
        .await;
        let reply = next_outbound(&mut rx).await;
        assert!(
            matches!(reply, Outbound::Reply(ref f) if matches!(f.payload, Payload::Error { .. })),
            "a running session is refused: {reply:?}"
        );
        assert!(state.hub.handle(&id).is_some());
        assert!(state.rc_keys.lock().unwrap().get(&id).is_some());
        assert!(state
            .store
            .list()
            .await
            .unwrap()
            .iter()
            .any(|meta| meta.id == id));
        state.hub.handle(&id).unwrap().interrupt();
    }

    /// LG-d: the snapshot's watermark is the last seq published before it,
    /// and the pump forwards only frames strictly after it.
    #[tokio::test]
    async fn attach_snapshot_watermark_is_the_seq_the_pump_starts_after() {
        let (state, _dir) = test_state();
        let id = state.store.create_session("attach").await.unwrap().id;
        let handle = state.hub.ensure_session(id, None);
        for n in 0..3 {
            handle.publish(Payload::Note {
                text: format!("n{n}"),
            });
        }

        let (mut ctx, mut rx) = authed_ctx();
        attach(&state, &mut ctx, Some(9), id).await;
        let snapshot = match next_outbound(&mut rx).await {
            Outbound::Reply(frame) => *frame,
            other => panic!("expected the snapshot first, got {other:?}"),
        };
        let last_seq = match snapshot.payload {
            Payload::Snapshot { last_seq, .. } => last_seq,
            other => panic!("expected a snapshot, got {other:?}"),
        };
        assert_eq!(last_seq, 3);
        assert_eq!(snapshot.seq, 3);

        handle.publish(Payload::Note {
            text: "live".into(),
        });
        match next_outbound(&mut rx).await {
            Outbound::Live(live) => assert_eq!(live.seq, 4, "first live frame after the watermark"),
            other => panic!("expected a live frame, got {other:?}"),
        }
    }

    /// ExplainRule is answered by a correlated Note whose text is the
    /// explain lines against the session's current rules, ending in result.
    #[tokio::test]
    async fn explain_rule_answers_a_correlated_note_with_the_verdict() {
        let (state, _dir) = test_state();
        let id = state.store.create_session("rules").await.unwrap().id;
        let handle = state.hub.ensure_session(id, None);
        handle
            .apply_config(&ConfigPatch {
                rules: Some(vec![modron_protocol::RuleWire {
                    effect: "deny".into(),
                    spec: "bash(rm -rf*)".into(),
                }]),
                ..ConfigPatch::default()
            })
            .unwrap();

        let (mut ctx, mut rx) = authed_ctx();
        handle_frame(
            &state,
            &mut ctx,
            true,
            Frame::request(
                7,
                Payload::ExplainRule {
                    session_id: id,
                    tool: "bash".into(),
                    args: serde_json::json!({"command": "rm -rf x"}),
                },
            ),
        )
        .await;
        match next_outbound(&mut rx).await {
            Outbound::Reply(frame) => {
                assert_eq!(frame.id, Some(7), "correlated to the request");
                match frame.payload {
                    Payload::Note { text } => assert_eq!(
                        text.lines().collect::<Vec<_>>(),
                        vec!["deny bash(rm -rf*): matched", "result: deny"]
                    ),
                    other => panic!("expected a note, got {other:?}"),
                }
            }
            other => panic!("expected a reply, got {other:?}"),
        }

        // An unknown session is NotFound, not a note.
        handle_frame(
            &state,
            &mut ctx,
            true,
            Frame::request(
                8,
                Payload::ExplainRule {
                    session_id: uuid::Uuid::new_v4(),
                    tool: "bash".into(),
                    args: serde_json::Value::Null,
                },
            ),
        )
        .await;
        assert!(matches!(
            next_outbound(&mut rx).await,
            Outbound::Reply(ref f) if matches!(f.payload, Payload::Error { .. })
        ));
    }

    /// A run that finishes successfully when the test sends on `done`.
    fn finishing_run(id: SessionId, done: tokio::sync::oneshot::Receiver<()>) -> RunFuture {
        Box::pin(async move {
            let _ = done.await;
            Ok(RunSummary {
                session_id: id,
                answer: "planned".into(),
                turns: 1,
                tool_calls: 0,
                usage: Default::default(),
                models_used: vec![],
            })
        })
    }

    fn error_message(outbound: Outbound) -> String {
        match outbound {
            Outbound::Reply(frame) => match frame.payload {
                Payload::Error { message, .. } => message,
                _ => panic!("expected an error reply"),
            },
            other => panic!("expected a reply, got {other:?}"),
        }
    }

    /// A session in plan phase whose plan run finished with one recorded
    /// plan item, so a decision is pending.
    async fn session_with_pending_plan(
        state: &Arc<ServerState>,
    ) -> (SessionId, Arc<crate::hub::SessionHandle>) {
        let id = state.store.create_session("plan").await.unwrap().id;
        let handle = state.hub.ensure_session(id, None);
        handle
            .apply_config(&ConfigPatch {
                phase: Some(Phase::Plan),
                ..ConfigPatch::default()
            })
            .unwrap();
        let (done, gate) = tokio::sync::oneshot::channel();
        state.hub.start_run(id, finishing_run(id, gate)).unwrap();
        handle.on_persisted(&modron_core::SessionEvent::Plan {
            items: vec![modron_core::PlanItem {
                text: "add the parser".into(),
                done_criteria: "parser tests pass".into(),
                status: "pending".into(),
            }],
        });
        done.send(()).unwrap();
        for _ in 0..200 {
            if handle.plan_pending() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(handle.plan_pending(), "the plan run proposed its plan");
        (id, handle)
    }

    /// Read live frames until a run's terminal frame, proving the decision
    /// started a run that ended (2s budget).
    async fn await_terminal_frame(rx: &mut tokio::sync::broadcast::Receiver<LiveFrame>) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let frame = rx.recv().await.expect("live frame");
                let json: serde_json::Value = serde_json::from_str(&frame.json).unwrap();
                if matches!(
                    json["type"].as_str(),
                    Some("turn_done" | "turn_failed" | "turn_aborted")
                ) {
                    return;
                }
            }
        })
        .await
        .expect("the decision started a run that ended");
    }

    #[test]
    fn plan_decision_task_text_and_note_rules() {
        assert_eq!(
            plan_decision_task(PlanDecision::Approve, None).unwrap(),
            "Execute the approved plan."
        );
        assert_eq!(
            plan_decision_task(PlanDecision::Approve, Some("keep it small".into())).unwrap(),
            "Execute the approved plan.\nkeep it small"
        );
        assert_eq!(
            plan_decision_task(PlanDecision::Approve, Some("  ".into())).unwrap(),
            "Execute the approved plan."
        );
        assert_eq!(
            plan_decision_task(PlanDecision::Revise, Some("split step two".into())).unwrap(),
            "split step two"
        );
        assert_eq!(
            plan_decision_task(PlanDecision::Revise, None).unwrap_err(),
            "revising a plan needs a note"
        );
    }

    #[tokio::test]
    async fn decide_plan_with_nothing_pending_errors() {
        let (state, _dir) = test_state();
        let id = state.store.create_session("idle").await.unwrap().id;
        state.hub.ensure_session(id, None);
        let (mut ctx, mut rx) = authed_ctx();
        handle_frame(
            &state,
            &mut ctx,
            true,
            Frame::request(
                1,
                Payload::DecidePlan {
                    session_id: id,
                    decision: PlanDecision::Approve,
                    note: None,
                },
            ),
        )
        .await;
        assert_eq!(error_message(next_outbound(&mut rx).await), NO_PLAN_PENDING);
        assert_eq!(
            state.hub.handle(&id).unwrap().engine_cell().config.phase,
            Phase::Build
        );
    }

    #[tokio::test]
    async fn revise_without_a_note_is_refused_and_keeps_the_plan_pending() {
        let (state, _dir) = test_state();
        let (id, handle) = session_with_pending_plan(&state).await;
        let (mut ctx, mut rx) = authed_ctx();
        handle_frame(
            &state,
            &mut ctx,
            true,
            Frame::request(
                2,
                Payload::DecidePlan {
                    session_id: id,
                    decision: PlanDecision::Revise,
                    note: Some("   ".into()),
                },
            ),
        )
        .await;
        assert!(error_message(next_outbound(&mut rx).await).contains("needs a note"));
        assert!(handle.plan_pending());
        assert_eq!(handle.engine_cell().config.phase, Phase::Plan);
    }

    /// Approve flips the phase to build (ConfigChanged before PlanDecided),
    /// clears the pending plan, and starts a run whose task is the approved
    /// plan line followed by the note.
    #[tokio::test]
    async fn approve_flips_to_build_clears_pending_and_runs_the_plan() {
        let (state, _dir) = test_state();
        let (id, handle) = session_with_pending_plan(&state).await;
        let mut live = handle.subscribe();
        let (mut ctx, mut rx) = authed_ctx();
        handle_frame(
            &state,
            &mut ctx,
            true,
            Frame::request(
                3,
                Payload::DecidePlan {
                    session_id: id,
                    decision: PlanDecision::Approve,
                    note: Some("keep it small".into()),
                },
            ),
        )
        .await;
        assert!(rx.try_recv().is_err(), "approve replies with no error");
        assert_eq!(handle.engine_cell().config.phase, Phase::Build);
        assert!(!handle.plan_pending());
        assert!(!handle.live_state().plan_pending);

        let mut kinds = Vec::new();
        while let Ok(frame) = live.try_recv() {
            let json: serde_json::Value = serde_json::from_str(&frame.json).unwrap();
            kinds.push(json["type"].as_str().unwrap_or_default().to_string());
        }
        let changed = kinds.iter().position(|k| k == "config_changed");
        let decided = kinds.iter().position(|k| k == "plan_decided");
        assert!(changed.is_some() && decided.is_some(), "{kinds:?}");
        assert!(changed < decided, "ConfigChanged precedes PlanDecided");
        // The run may already have ended before the drain above.
        if !kinds.iter().any(|k| k.starts_with("turn_")) {
            await_terminal_frame(&mut live).await;
        }
    }

    /// Revise keeps the plan phase, clears the pending plan, and starts
    /// another plan run with the note as its task.
    #[tokio::test]
    async fn revise_keeps_the_plan_phase_and_replans_with_the_note() {
        let (state, _dir) = test_state();
        let (id, handle) = session_with_pending_plan(&state).await;
        let (mut ctx, mut rx) = authed_ctx();
        let mut live = handle.subscribe();
        handle_frame(
            &state,
            &mut ctx,
            true,
            Frame::request(
                4,
                Payload::DecidePlan {
                    session_id: id,
                    decision: PlanDecision::Revise,
                    note: Some("split step two".into()),
                },
            ),
        )
        .await;
        assert!(rx.try_recv().is_err(), "revise replies with no error");
        assert_eq!(handle.engine_cell().config.phase, Phase::Plan);
        assert!(!handle.plan_pending());
        await_terminal_frame(&mut live).await;
    }

    fn catalog_entry(
        name: &str,
        provider: &str,
        tier: modron_router::Tier,
    ) -> modron_router::ModelInfo {
        modron_router::ModelInfo {
            name: name.into(),
            provider: provider.into(),
            context_window: 128_000,
            input_per_m: 0.0,
            output_per_m: 0.0,
            tier,
        }
    }

    #[test]
    fn models_sort_available_first_then_provider_then_id() {
        use modron_router::Tier::{Cheap, Strong};
        let catalog = [
            catalog_entry("zeta-strong", "beta", Strong),
            catalog_entry("gamma-x", "gamma", Cheap),
            catalog_entry("beta-cheap", "beta", Cheap),
            catalog_entry("alpha-cheap", "alpha", Cheap),
        ];
        let rows = models_for(&catalog, |p| p == "alpha" || p == "beta");
        let order: Vec<(&str, bool)> = rows.iter().map(|r| (r.id.as_str(), r.available)).collect();
        assert_eq!(
            order,
            vec![
                ("alpha-cheap", true),
                ("beta-cheap", true),
                ("zeta-strong", true),
                ("gamma-x", false),
            ]
        );
        // Label falls back to the id; tier and window come from the catalog.
        assert_eq!(rows[0].label, "alpha-cheap");
        assert_eq!(rows[0].provider, "alpha");
        assert_eq!(rows[0].context_window, Some(128_000));
        assert_eq!(rows[0].tier.as_deref(), Some("cheap"));
        assert_eq!(rows[2].tier.as_deref(), Some("strong"));
    }

    #[test]
    fn models_over_the_real_catalog_cover_every_entry_once() {
        let catalog = modron_router::catalog();
        let rows = models_for(catalog, |_| false);
        assert_eq!(rows.len(), catalog.len());
        assert!(rows.iter().all(|r| !r.available));
        let mut ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), catalog.len(), "catalog model names are unique");
        // With every provider signed in, nothing sorts after an unavailable row.
        let all = models_for(catalog, |_| true);
        assert!(all.iter().all(|r| r.available));
    }
}
