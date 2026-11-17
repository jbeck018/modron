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
    ConfigPatch, ForkOriginWire, Frame, ModelWire, Payload, PlanDecision, SessionMetaWire,
    PROTOCOL_VERSION,
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
    /// True only for a remote-control tunnel that authenticated to its
    /// bridge with the host token (`MODRON_HOST_TOKEN`). Such a tunnel
    /// trusts the `role` each relayed frame carries (see
    /// [`handle_relayed_frame`] and [`relay_gate`]). Every other connection
    /// ignores `Frame::role`.
    pub trusted_relay: bool,
}

impl ConnCtx {
    pub fn new(out: mpsc::UnboundedSender<Outbound>) -> Self {
        Self {
            out,
            backlog: None,
            authed: false,
            subscription: None,
            scope: None,
            trusted_relay: false,
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
            trusted_relay: false,
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
            trusted_relay: false,
        }
    }

    /// A tunnel that authenticated to its bridge with the host token: its
    /// relayed frames carry a trusted `role`, judged by [`relay_gate`].
    pub fn trusted_tunnel(out: mpsc::UnboundedSender<Outbound>, session: SessionId) -> Self {
        let mut ctx = Self::scoped(out, session);
        ctx.trusted_relay = true;
        ctx
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

/// The least role a relayed frame needs on a trusted tunnel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RelayTier {
    /// Connection plumbing and reads of this session: any role.
    Viewer,
    /// Answers to this session's prompts: approver or owner.
    Approver,
    /// Anything that acts on this session: owner only.
    Owner,
}

const OTHER_SESSION: &str = "this connection is scoped to another session";
const SERVER_WIDE: &str = "this request is not available through a remote-control tunnel";

/// The tier of every client-to-server payload on a trusted tunnel pinned to
/// `scope`. Exhaustive on purpose, with no wildcard: a new `Payload` variant
/// does not compile until it has a decision here. Server-to-client payloads
/// are refused (protocol misuse).
///
/// Owner (everything this session's local owner may do, nothing else):
/// interrupt, submit to this session, steer / steer_agent / kill_agent,
/// enqueue / dequeue, compact, wake / cancel_wake, set_config (the attached
/// session, which is `scope`), explain_rule, fork of this session,
/// get_task_status of this session, shell, and the read requests models,
/// router_defaults and auth_providers.
///
/// Approver: approve (resolved only inside `scope`) and decide_plan.
///
/// Viewer: hello, ping, detach and attach to `scope`.
///
/// Refused for every role: list / delete / create sessions and tasks,
/// submit that starts a new session or names another one, attach to another
/// session, merge_task (it writes the repository's base branch), automations,
/// remote_control, publish_page, auth_start, auth_set_key, auth_remove,
/// persist_router and
/// persist_default_model (server-wide settings and credentials). Shell is
/// Owner-only: the owner's own `!` command, never a guest's.
pub(crate) fn relay_tier(scope: SessionId, payload: &Payload) -> Result<RelayTier, &'static str> {
    use RelayTier::*;
    let own = |id: &SessionId| *id == scope;
    match payload {
        Payload::Hello { .. } | Payload::Ping {} | Payload::Detach {} => Ok(Viewer),
        Payload::Attach { session_id, .. } => {
            if own(session_id) {
                Ok(Viewer)
            } else {
                Err(OTHER_SESSION)
            }
        }
        Payload::Approve { .. } => Ok(Approver),
        Payload::DecidePlan { session_id, .. } => {
            if own(session_id) {
                Ok(Approver)
            } else {
                Err(OTHER_SESSION)
            }
        }
        Payload::Submit { session_id, .. } => match session_id {
            Some(id) if own(id) => Ok(Owner),
            Some(_) => Err(OTHER_SESSION),
            None => Err("a new session cannot be started through a remote-control tunnel"),
        },
        Payload::Interrupt { session_id }
        | Payload::Shell { session_id, .. }
        | Payload::SteerAgent { session_id, .. }
        | Payload::KillAgent { session_id, .. }
        | Payload::Steer { session_id, .. }
        | Payload::Enqueue { session_id, .. }
        | Payload::Dequeue { session_id, .. }
        | Payload::Compact { session_id }
        | Payload::Wake { session_id, .. }
        | Payload::CancelWake { session_id }
        | Payload::ExplainRule { session_id, .. }
        | Payload::Fork { session_id, .. }
        | Payload::GetTaskStatus { session_id } => {
            if own(session_id) {
                Ok(Owner)
            } else {
                Err(OTHER_SESSION)
            }
        }
        Payload::SetConfig { .. }
        | Payload::RouterDefaultsReq {}
        | Payload::ModelsReq {}
        // The persona list is the same class as the model catalog: owner
        // only on a relayed tunnel (approvers and viewers do not pick
        // personas).
        | Payload::PersonasReq {}
        | Payload::AuthProvidersReq {} => Ok(Owner),
        Payload::NewSession { .. }
        | Payload::ListSessions {}
        | Payload::DeleteSession { .. }
        | Payload::CreateTask { .. }
        | Payload::MergeTask { .. }
        | Payload::AutomationsList {}
        | Payload::AutomationAdd { .. }
        | Payload::AutomationRemove { .. }
        | Payload::RemoteControl { .. }
        | Payload::PublishPage { .. }
        | Payload::AuthStart { .. }
        | Payload::AuthSetKey { .. }
        | Payload::AuthRemove { .. }
        | Payload::PersistRouter { .. }
        | Payload::PersistDefaultModel { .. } => Err(SERVER_WIDE),
        Payload::Welcome { .. }
        | Payload::Snapshot { .. }
        | Payload::Event { .. }
        | Payload::Delta { .. }
        | Payload::Tool { .. }
        | Payload::ApprovalRequested { .. }
        | Payload::ApprovalResolved { .. }
        | Payload::Steered { .. }
        | Payload::QueueChanged { .. }
        | Payload::ContextChanged { .. }
        | Payload::WakeChanged { .. }
        | Payload::Notify { .. }
        | Payload::AgentFinished { .. }
        | Payload::ConfigChanged { .. }
        | Payload::PlanProposed { .. }
        | Payload::PlanDecided { .. }
        | Payload::Sessions { .. }
        | Payload::TaskCreated { .. }
        | Payload::TaskStatus { .. }
        | Payload::TaskMerged { .. }
        | Payload::RemoteControlStarted { .. }
        | Payload::Automations { .. }
        | Payload::PagePublished { .. }
        | Payload::AuthProviders { .. }
        | Payload::TurnDone { .. }
        | Payload::TurnFailed { .. }
        | Payload::TurnAborted {}
        | Payload::OAuthProgress { .. }
        | Payload::RouterDefaults { .. }
        | Payload::Models { .. }
        | Payload::Personas { .. }
        | Payload::Note { .. }
        | Payload::Pong {}
        | Payload::Error { .. } => Err("frames of this type are server-to-client"),
    }
}

/// Whether a role on a trusted tunnel may send a frame of `tier`.
pub(crate) fn role_allows(role: modron_protocol::RelayRole, tier: RelayTier) -> bool {
    use modron_protocol::RelayRole::*;
    matches!(
        (role, tier),
        (_, RelayTier::Viewer)
            | (Owner | Approver, RelayTier::Approver)
            | (Owner, RelayTier::Owner)
    )
}

/// The per-frame decision for a relayed frame on a TRUSTED tunnel pinned to
/// `scope`. A frame without a role is refused: the portal bridge always
/// sets one. The role is trusted only because the tunnel authenticated with
/// the host token (see [`ConnCtx::trusted_tunnel`]).
pub(crate) fn relay_gate(
    scope: SessionId,
    role: Option<modron_protocol::RelayRole>,
    payload: &Payload,
) -> Result<(), &'static str> {
    let tier = relay_tier(scope, payload)?;
    let Some(role) = role else {
        return Err("this relayed frame carries no role");
    };
    if role_allows(role, tier) {
        Ok(())
    } else {
        Err(match tier {
            RelayTier::Owner => "this request is only for the owner",
            RelayTier::Approver => "this request needs the approver role or the owner",
            RelayTier::Viewer => "this role may not send this request",
        })
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
        role: None,
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
///
/// Server-generated frames (the tunnel's own hello, attach and ping) and
/// local connections come through here. A scoped connection passes the
/// legacy [`scoped_gate`] first; `Frame::role` is never read on this path.
pub async fn handle_frame(
    state: &Arc<ServerState>,
    ctx: &mut ConnCtx,
    trusted: bool,
    frame: Frame,
) -> Flow {
    if let Some(scope) = ctx.scope {
        if let Err(message) = scoped_gate(scope, &frame.payload) {
            send(
                ctx,
                error_kind(frame.id, message, modron_protocol::ErrorKind::Forbidden),
            );
            return Flow::Continue;
        }
    }
    dispatch_frame(state, ctx, trusted, frame).await
}

/// Handle one frame relayed by a remote-control tunnel from its bridge.
///
/// A tunnel without the host token (the legacy share-link relay) is handled
/// exactly as before: `frame.role` is ignored. A trusted tunnel decides per
/// frame with [`relay_gate`] on the relayed role, and the legacy scope gate
/// does not apply to a frame that passes it.
pub async fn handle_relayed_frame(
    state: &Arc<ServerState>,
    ctx: &mut ConnCtx,
    frame: Frame,
) -> Flow {
    let Some(scope) = ctx.scope.filter(|_| ctx.trusted_relay) else {
        return handle_frame(state, ctx, true, frame).await;
    };
    if let Err(message) = relay_gate(scope, frame.role, &frame.payload) {
        send(
            ctx,
            error_kind(frame.id, message, modron_protocol::ErrorKind::Forbidden),
        );
        return Flow::Continue;
    }
    dispatch_frame(state, ctx, true, frame).await
}

async fn dispatch_frame(
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
                        // The project commands of the served directory (the
                        // engine expands the same files on a run).
                        commands: modron_core::context::project_command_names(&state.working_dir),
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
                            forked_from: meta.forked_from.map(|origin| ForkOriginWire {
                                session_id: origin.parent,
                                turn: origin.turn,
                            }),
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
        Payload::AuthRemove {
            session_id,
            provider,
        } => {
            handle_auth_remove(state, ctx, request_id, session_id, provider);
        }
        Payload::RouterDefaultsReq {} => {
            handle_router_defaults_req(state, ctx, request_id).await;
        }
        Payload::ModelsReq {} => {
            handle_models_req(state, ctx, request_id);
        }
        Payload::PersonasReq {} => {
            handle_personas_req(state, ctx, request_id);
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
        Payload::PersistDefaultModel { model, provider } => {
            handle_persist_default_model(state, ctx, request_id, model, provider);
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
        // `!command` passthrough (PARITY-C): one bash call through the same
        // admission and execution as a model call (Engine::run_user_shell),
        // run in the session's run slot so it is busy, interruptible and
        // approval-gated like a turn. Ends with TurnDone or TurnFailed.
        Payload::Shell {
            session_id,
            command,
            private,
        } => {
            if command.trim().is_empty() {
                send(ctx, error(request_id, "usage: !<shell command>"));
            } else {
                match state.hub.handle(&session_id) {
                    None => send(
                        ctx,
                        error_kind(
                            request_id,
                            format!("no such session {session_id}"),
                            modron_protocol::ErrorKind::NotFound,
                        ),
                    ),
                    Some(handle) if handle.is_busy() => send(
                        ctx,
                        error_kind(
                            request_id,
                            "a run is active in this session; shell passthrough waits for it",
                            modron_protocol::ErrorKind::Conflict,
                        ),
                    ),
                    Some(handle) => {
                        let engine = handle.engine_cell().clone();
                        let run: crate::hub::ShellRunFuture = Box::pin(async move {
                            engine.run_user_shell(session_id, &command, private).await
                        });
                        match state.hub.start_shell_run(session_id, run) {
                            Err(message) => send(ctx, error(request_id, message)),
                            Ok(()) => {
                                // Like Submit: a connection with no
                                // subscription watches the session it ran in.
                                if ctx.subscription.is_none() {
                                    attach(state, ctx, None, session_id).await;
                                }
                            }
                        }
                    }
                }
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
        Payload::Fork { session_id, turn } => {
            handle_fork(state, ctx, request_id, session_id, turn).await;
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

/// The persona rows for `personas_req`: the configured personas sorted by
/// name, each with its forced model and a one-line summary. The full system
/// prompt never leaves the server.
fn handle_personas_req(state: &Arc<ServerState>, ctx: &mut ConnCtx, request_id: Option<u64>) {
    let personas = persona_rows(&state.hub.personas());
    send(ctx, reply(request_id, None, Payload::Personas { personas }));
}

/// The wire rows for a persona map (the map is already sorted by name).
pub(crate) fn persona_rows(
    personas: &std::collections::BTreeMap<String, modron_core::PersonaSpec>,
) -> Vec<modron_protocol::PersonaWire> {
    personas
        .iter()
        .map(|(name, spec)| modron_protocol::PersonaWire {
            name: name.clone(),
            model: spec.model.clone(),
            summary: persona_summary(spec.system.as_deref()),
        })
        .collect()
}

/// The first line of a system prompt, trimmed, display-sanitized and capped
/// at 80 characters. Empty when the persona has no prompt.
pub(crate) fn persona_summary(system: Option<&str>) -> String {
    let first = system.unwrap_or("").lines().next().unwrap_or("").trim();
    modron_view::sanitize_display(first)
        .chars()
        .take(PERSONA_SUMMARY_MAX)
        .collect()
}

/// Character cap for a persona summary on the wire.
pub(crate) const PERSONA_SUMMARY_MAX: usize = 80;

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
    turn: Option<u32>,
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
        Ok(events) => {
            // An out-of-range turn is refused before any copy, with the
            // plain core message (the turn list is the caller's own input).
            if let Err(e) = modron_core::rewind::fork_cut(&events, turn) {
                let message = match e {
                    modron_core::Error::Session(text) => text,
                    other => other.to_string(),
                };
                send(ctx, error(request_id, message));
                return;
            }
            match state.store.fork_at(session_id, turn).await {
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
            }
        }
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

/// AuthRemove (local connections only, like auth_set_key): delete what
/// Modron stored for a provider (the env-file key line and Modron's own
/// OAuth tokens), then answer with a correlated `note` and a fresh
/// `auth_providers` frame so the rows update.
///
/// A running session is not touched: its engine holds the provider object
/// it was built with, which carries the key or tokens read at that time.
/// The env file is read again only when a provider is built, and the
/// Anthropic subscription provider re-reads its token file only on refresh.
fn handle_auth_remove(
    _state: &Arc<ServerState>,
    ctx: &mut ConnCtx,
    request_id: Option<u64>,
    _session_id: SessionId,
    provider: String,
) {
    let Some(info) = modron_core::auth::provider_info(&provider) else {
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
    match modron_core::auth::remove_credential(info.provider_id) {
        Ok(removed) => {
            if removed.key_removed {
                std::env::remove_var(info.env_var);
            }
            let mut text = format!("{}: {}", info.name, removed.describe());
            if std::env::var(info.env_var).is_ok() {
                text.push_str(
                    "; a key is still set in this server's environment (unset it where modron serve starts)",
                );
            }
            if !removed.nothing_stored() {
                text.push_str("; sessions already running keep the provider they were built with");
            }
            send(ctx, reply(request_id, None, Payload::Note { text }));
            // Uncorrelated on purpose: the request id was answered by the
            // note above. Clients fold this frame by its payload.
            send(
                ctx,
                reply(
                    None,
                    None,
                    Payload::AuthProviders {
                        rows: auth_provider_rows(),
                    },
                ),
            );
        }
        Err(message) => send(ctx, error(request_id, message)),
    }
}

/// Why a default model/provider pair cannot be saved, or `None` when it can.
/// Both strings must be non-empty and free of control characters, and the
/// provider must be one the server knows: a provider in the auth registry
/// or a provider a catalog model names (the same sources `models_req` and
/// `auth_set_key` read).
pub(crate) fn default_model_problem(model: &str, provider: &str) -> Option<String> {
    for (label, value) in [("model", model), ("provider", provider)] {
        if value.trim().is_empty() {
            return Some(format!("default {label} is empty"));
        }
        if value.chars().any(char::is_control) {
            return Some(format!("default {label} contains a control character"));
        }
    }
    let registered = modron_core::auth::provider_registry()
        .iter()
        .any(|info| info.provider_id == provider);
    let catalogued = modron_router::catalog()
        .iter()
        .any(|info| info.provider == provider);
    if registered || catalogued {
        None
    } else {
        Some(format!("unknown provider {provider}"))
    }
}

/// PersistDefaultModel (decision 3, PLAN.md 10.2a): write the server's
/// default model and provider to config.toml, then apply them to the engine
/// template future sessions start from. The current session is untouched.
fn handle_persist_default_model(
    state: &Arc<ServerState>,
    ctx: &mut ConnCtx,
    request_id: Option<u64>,
    model: String,
    provider: String,
) {
    if let Some(problem) = default_model_problem(&model, &provider) {
        send(ctx, error(request_id, problem));
        return;
    }
    match crate::tasks::persist_default_model_toml(&model, &provider) {
        Ok(()) => {
            state.hub.apply_default_model(&model, &provider);
            send(
                ctx,
                reply(
                    request_id,
                    None,
                    Payload::Note {
                        text: format!("default model saved: {model} ({provider}) for new sessions"),
                    },
                ),
            );
        }
        Err(e) => send(ctx, error(request_id, e.to_string())),
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
    // Provider blocks never leave the server (H7b-2): strip at the send.
    let mut events = events;
    events
        .iter_mut()
        .for_each(modron_core::SessionEvent::strip_provider_blocks);
    let snapshot = Frame {
        id: request_id,
        session: Some(session_id),
        seq: watermark,
        role: None,
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
        Payload::Shell { .. } => "shell",
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
        Payload::PersonasReq {} => "personas_req",
        Payload::Personas { .. } => "personas",
        Payload::AuthSetKey { .. } => "auth_set_key",
        Payload::AuthRemove { .. } => "auth_remove",
        Payload::AuthProviders { .. } | Payload::AuthProvidersReq {} => "auth_providers",
        Payload::PersistRouter { .. } => "persist_router",
        Payload::PersistDefaultModel { .. } => "persist_default_model",
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
        test_state_with(Vec::new(), Default::default())
    }

    /// [`test_state`] with the given tools and approval policy on the
    /// template engine every session is built from.
    fn test_state_with(
        tools: Vec<Arc<dyn modron_core::Tool>>,
        approvals: modron_core::ApprovalPolicy,
    ) -> (Arc<ServerState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let hub = SessionHub::empty();
        let inner: Arc<dyn SessionStore> = Arc::new(modron_store::JsonlSessionStore::new(
            dir.path().join("sessions"),
        ));
        let store: Arc<dyn SessionStore> = Arc::new(BroadcastingStore::new(inner, hub.clone()));
        let mut registry = ToolRegistry::new();
        for tool in tools {
            registry.register(tool);
        }
        hub.set_engine(Engine {
            selector: Arc::new(StaticSelector::new("mock-model", "mock")),
            providers: Default::default(),
            tools: Arc::new(registry),
            store: Arc::clone(&store),
            config: EngineConfig {
                default_model: "mock-model".into(),
                default_provider: "mock".into(),
                approvals,
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

    /// Subscribe `ctx` to the session before a run starts, so no live frame
    /// (the approval prompt, the terminal frame) can race the subscription.
    /// Consumes the attach snapshot.
    async fn subscribe_first(
        state: &Arc<ServerState>,
        ctx: &mut ConnCtx,
        rx: &mut mpsc::UnboundedReceiver<Outbound>,
        id: SessionId,
    ) {
        attach(state, ctx, None, id).await;
        let snapshot = next_outbound(rx).await;
        assert!(
            matches!(snapshot, Outbound::Reply(_)),
            "attach snapshot first"
        );
    }

    /// Collect the `type` of each live frame until `terminal` arrives (or a
    /// timeout), returning every type seen in order.
    async fn collect_live_types_until(
        rx: &mut mpsc::UnboundedReceiver<Outbound>,
        terminal: &str,
    ) -> Vec<String> {
        let mut types = Vec::new();
        loop {
            let outbound = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("a live frame before the timeout")
                .expect("channel open");
            if let Outbound::Live(live) = outbound {
                let json: serde_json::Value = serde_json::from_str(&live.json).unwrap();
                let kind = json["type"].as_str().unwrap_or_default().to_string();
                types.push(kind.clone());
                if kind == terminal {
                    return types;
                }
            }
        }
    }

    /// A stand-in `bash` tool for shell-passthrough tests: returns a fixed
    /// output after an optional delay, counting its runs.
    struct ShellStub {
        output: String,
        delay: Duration,
        runs: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl modron_core::Tool for ShellStub {
        fn spec(&self) -> modron_core::ToolSpec {
            modron_core::ToolSpec {
                name: "bash".into(),
                description: "stub".into(),
                parameters: serde_json::json!({"type": "object"}),
            }
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &modron_core::ToolCtx,
        ) -> modron_core::Result<modron_core::ToolOutcome> {
            self.runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            Ok(modron_core::ToolOutcome {
                content: self.output.clone(),
                is_error: false,
                usage: None,
                meta: None,
            })
        }
    }

    fn stub_bash(
        output: &str,
        delay: Duration,
    ) -> (
        Arc<dyn modron_core::Tool>,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tool = Arc::new(ShellStub {
            output: output.into(),
            delay,
            runs: Arc::clone(&runs),
        });
        (tool, runs)
    }

    fn allow_all() -> modron_core::ApprovalPolicy {
        modron_core::ApprovalPolicy {
            mode: modron_core::ApprovalMode::ApproveAll,
            ..Default::default()
        }
    }

    /// PARITY-C: `Shell` on an idle session runs the command in the run
    /// slot, persists its output as a shared UserShell event, and ends with the
    /// normal TurnDone frame and no Notify.
    #[tokio::test]
    async fn shell_on_an_idle_session_runs_and_ends_with_turn_done() {
        let (tool, runs) = stub_bash("hi\nexit code: 0", Duration::ZERO);
        let (state, _dir) = test_state_with(vec![tool], allow_all());
        let id = state.store.create_session("shell").await.unwrap().id;
        state.hub.ensure_session(id, None);
        let (mut ctx, mut rx) = authed_ctx();
        subscribe_first(&state, &mut ctx, &mut rx, id).await;

        handle_frame(
            &state,
            &mut ctx,
            true,
            Frame::request(
                1,
                Payload::Shell {
                    session_id: id,
                    command: "echo hi".into(),
                    private: false,
                },
            ),
        )
        .await;
        let types = collect_live_types_until(&mut rx, "turn_done").await;
        assert!(!types.contains(&"notify".to_string()), "{types:?}");
        assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(!state.hub.handle(&id).unwrap().is_busy(), "slot released");

        let shell: Vec<modron_core::SessionEvent> = state
            .store
            .load(id)
            .await
            .unwrap()
            .into_iter()
            .filter(|event| matches!(event, modron_core::SessionEvent::UserShell { .. }))
            .collect();
        assert!(matches!(
            &shell[..],
            [modron_core::SessionEvent::UserShell {
                command,
                output,
                private: false,
                ..
            }] if command == "echo hi" && output == "hi\nexit code: 0"
        ));
    }

    /// A `!command` the approval policy denies ends its run with a Note
    /// carrying the reason, not a Notify or a failure.
    #[tokio::test]
    async fn shell_refused_by_policy_ends_with_a_note_and_turn_done() {
        let (tool, runs) = stub_bash("never", Duration::ZERO);
        let policy = modron_core::ApprovalPolicy {
            deny: vec!["bash".into()],
            ..allow_all()
        };
        let (state, _dir) = test_state_with(vec![tool], policy);
        let id = state.store.create_session("shell").await.unwrap().id;
        state.hub.ensure_session(id, None);
        let (mut ctx, mut rx) = authed_ctx();
        subscribe_first(&state, &mut ctx, &mut rx, id).await;
        handle_frame(
            &state,
            &mut ctx,
            true,
            Frame::request(
                1,
                Payload::Shell {
                    session_id: id,
                    command: "ls".into(),
                    private: false,
                },
            ),
        )
        .await;
        let types = collect_live_types_until(&mut rx, "turn_done").await;
        assert!(types.contains(&"note".to_string()), "{types:?}");
        assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    /// Interrupt aborts a running shell command and publishes TurnAborted,
    /// and the slot is free afterwards.
    #[tokio::test]
    async fn interrupt_aborts_a_running_shell_command() {
        let (tool, _runs) = stub_bash("late", Duration::from_secs(5));
        let (state, _dir) = test_state_with(vec![tool], allow_all());
        let id = state.store.create_session("shell").await.unwrap().id;
        state.hub.ensure_session(id, None);
        let (mut ctx, mut rx) = authed_ctx();
        subscribe_first(&state, &mut ctx, &mut rx, id).await;
        handle_frame(
            &state,
            &mut ctx,
            true,
            Frame::request(
                1,
                Payload::Shell {
                    session_id: id,
                    command: "sleep 5".into(),
                    private: false,
                },
            ),
        )
        .await;
        // Wait until the run is live, then interrupt.
        for _ in 0..200 {
            if state.hub.handle(&id).unwrap().is_busy() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        handle_frame(
            &state,
            &mut ctx,
            true,
            Frame::request(2, Payload::Interrupt { session_id: id }),
        )
        .await;
        let types = collect_live_types_until(&mut rx, "turn_aborted").await;
        assert!(!types.contains(&"turn_done".to_string()), "{types:?}");
        assert!(!state.hub.handle(&id).unwrap().is_busy());
    }

    /// PLAN.md 10.2a decision 2: the owner's own `!` command is never
    /// approval-prompted, even under Ask with no standing allow. It runs,
    /// and no approval_requested frame is published.
    #[tokio::test(flavor = "multi_thread")]
    async fn shell_runs_without_an_approval_prompt_under_ask() {
        let (tool, runs) = stub_bash("listed", Duration::ZERO);
        let policy = modron_core::ApprovalPolicy {
            mode: modron_core::ApprovalMode::Ask,
            ..Default::default()
        };
        let (state, _dir) = test_state_with(vec![tool], policy);
        let id = state.store.create_session("shell").await.unwrap().id;
        state.hub.ensure_session(id, None);
        let (mut ctx, mut rx) = authed_ctx();
        subscribe_first(&state, &mut ctx, &mut rx, id).await;
        handle_frame(
            &state,
            &mut ctx,
            true,
            Frame::request(
                1,
                Payload::Shell {
                    session_id: id,
                    command: "ls".into(),
                    private: false,
                },
            ),
        )
        .await;
        let types = collect_live_types_until(&mut rx, "turn_done").await;
        assert!(
            !types.contains(&"approval_requested".to_string()),
            "{types:?}"
        );
        assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// The default-model problem check: empty, control characters and an
    /// unknown provider are refused; a registered provider and a catalog
    /// provider pass.
    #[test]
    fn default_model_validation_refuses_bad_values() {
        assert!(default_model_problem("", "anthropic")
            .unwrap()
            .contains("model is empty"));
        assert!(default_model_problem("m", "  ")
            .unwrap()
            .contains("provider is empty"));
        assert!(default_model_problem("m\n", "anthropic")
            .unwrap()
            .contains("control character"));
        assert!(default_model_problem("m", "no-such-provider")
            .unwrap()
            .contains("unknown provider"));
        assert_eq!(default_model_problem("m", "anthropic"), None);
        let catalog = modron_router::catalog();
        let catalogued = catalog.first().expect("catalog is not empty");
        assert_eq!(
            default_model_problem(&catalogued.name, &catalogued.provider),
            None
        );
    }

    /// A scoped (share or tunnel guest) connection may not run `!`, even in
    /// its own session: the scope does not tell a host from a guest, and
    /// guests never get a shell. Submit keeps its own-session rule.
    #[test]
    fn scoped_connection_refuses_shell() {
        let own = SessionId::new_v4();
        let shell = Payload::Shell {
            session_id: own,
            command: "ls".into(),
            private: false,
        };
        assert!(scoped_gate(own, &shell).is_err());
    }

    /// One sample per client-to-server payload shape, with the decision the
    /// tunnel gives it: `[legacy scope gate, trusted owner, trusted approver,
    /// trusted viewer]`. Adding a client variant without a row fails
    /// `relay_table_covers_every_client_variant`, and the exhaustive match in
    /// `relay_tier` fails to compile first.
    fn relay_rows(own: SessionId, other: SessionId) -> Vec<(&'static str, Payload, [bool; 4])> {
        use modron_protocol::{Decision, PlanDecision};
        let text = || "text".to_string();
        vec![
            (
                "hello",
                Payload::Hello {
                    client: "c".into(),
                    protocol: PROTOCOL_VERSION,
                    token: None,
                },
                [true, true, true, true],
            ),
            ("ping", Payload::Ping {}, [true, true, true, true]),
            ("detach", Payload::Detach {}, [true, true, true, true]),
            (
                "attach own",
                Payload::Attach {
                    session_id: own,
                    last_seq: 0,
                },
                [true, true, true, true],
            ),
            (
                "attach other",
                Payload::Attach {
                    session_id: other,
                    last_seq: 0,
                },
                [false, false, false, false],
            ),
            (
                "approve",
                Payload::Approve {
                    request_id: "r".into(),
                    decision: Decision::AllowOnce,
                },
                [true, true, true, false],
            ),
            (
                "decide_plan own",
                Payload::DecidePlan {
                    session_id: own,
                    decision: PlanDecision::Approve,
                    note: None,
                },
                [true, true, true, false],
            ),
            (
                "decide_plan other",
                Payload::DecidePlan {
                    session_id: other,
                    decision: PlanDecision::Approve,
                    note: None,
                },
                [false, false, false, false],
            ),
            (
                "submit own",
                Payload::Submit {
                    session_id: Some(own),
                    task: text(),
                    attachments: vec![],
                },
                [true, true, false, false],
            ),
            (
                "submit other",
                Payload::Submit {
                    session_id: Some(other),
                    task: text(),
                    attachments: vec![],
                },
                [false, false, false, false],
            ),
            (
                "submit new session",
                Payload::Submit {
                    session_id: None,
                    task: text(),
                    attachments: vec![],
                },
                [false, false, false, false],
            ),
            (
                "interrupt own",
                Payload::Interrupt { session_id: own },
                [true, true, false, false],
            ),
            (
                "shell own",
                Payload::Shell {
                    session_id: own,
                    command: "ls".into(),
                    private: false,
                },
                [false, true, false, false],
            ),
            (
                "steer_agent own",
                Payload::SteerAgent {
                    session_id: own,
                    agent: "a".into(),
                    text: text(),
                },
                [true, true, false, false],
            ),
            (
                "kill_agent own",
                Payload::KillAgent {
                    session_id: own,
                    agent: "a".into(),
                },
                [true, true, false, false],
            ),
            (
                "steer own",
                Payload::Steer {
                    session_id: own,
                    text: text(),
                },
                [true, true, false, false],
            ),
            (
                "enqueue own",
                Payload::Enqueue {
                    session_id: own,
                    text: text(),
                },
                [true, true, false, false],
            ),
            (
                "dequeue own",
                Payload::Dequeue {
                    session_id: own,
                    index: 0,
                },
                [true, true, false, false],
            ),
            (
                "compact own",
                Payload::Compact { session_id: own },
                [true, true, false, false],
            ),
            (
                "wake own",
                Payload::Wake {
                    session_id: own,
                    after_minutes: Some(5),
                    every_minutes: None,
                    prompt: text(),
                },
                [true, true, false, false],
            ),
            (
                "cancel_wake own",
                Payload::CancelWake { session_id: own },
                [true, true, false, false],
            ),
            (
                "set_config",
                Payload::SetConfig {
                    patch: Default::default(),
                },
                [false, true, false, false],
            ),
            (
                "explain_rule own",
                Payload::ExplainRule {
                    session_id: own,
                    tool: "bash".into(),
                    args: serde_json::Value::Null,
                },
                [true, true, false, false],
            ),
            (
                "fork own",
                Payload::Fork {
                    session_id: own,
                    turn: None,
                },
                [false, true, false, false],
            ),
            (
                "fork other",
                Payload::Fork {
                    session_id: other,
                    turn: None,
                },
                [false, false, false, false],
            ),
            (
                "new_session",
                Payload::NewSession { title: None },
                [false, false, false, false],
            ),
            (
                "list_sessions",
                Payload::ListSessions {},
                [false, false, false, false],
            ),
            (
                "delete_session own",
                Payload::DeleteSession { session_id: own },
                [false, false, false, false],
            ),
            (
                "create_task",
                Payload::CreateTask {
                    title: text(),
                    base: None,
                },
                [false, false, false, false],
            ),
            (
                "get_task_status own",
                Payload::GetTaskStatus { session_id: own },
                [false, true, false, false],
            ),
            (
                "merge_task own",
                Payload::MergeTask {
                    session_id: own,
                    message: None,
                },
                [false, false, false, false],
            ),
            (
                "automations_list",
                Payload::AutomationsList {},
                [false, false, false, false],
            ),
            (
                "automation_add",
                Payload::AutomationAdd {
                    name: "n".into(),
                    prompt: text(),
                    interval_minutes: 5,
                },
                [false, false, false, false],
            ),
            (
                "automation_remove",
                Payload::AutomationRemove { name: "n".into() },
                [false, false, false, false],
            ),
            (
                "remote_control",
                Payload::RemoteControl {
                    session_id: own,
                    bridge_url: "https://b".into(),
                },
                [false, false, false, false],
            ),
            (
                "publish_page",
                Payload::PublishPage {
                    session_id: own,
                    title: text(),
                    content_md: text(),
                },
                [false, false, false, false],
            ),
            (
                "auth_providers_req",
                Payload::AuthProvidersReq {},
                [false, true, false, false],
            ),
            (
                "auth_start",
                Payload::AuthStart {
                    session_id: own,
                    provider: "p".into(),
                },
                [false, false, false, false],
            ),
            (
                "auth_set_key",
                Payload::AuthSetKey {
                    session_id: own,
                    provider: "p".into(),
                    key: "k".into(),
                },
                [false, false, false, false],
            ),
            (
                "auth_remove",
                Payload::AuthRemove {
                    session_id: own,
                    provider: "p".into(),
                },
                [false, false, false, false],
            ),
            (
                "persist_router",
                Payload::PersistRouter {
                    cheap: "a".into(),
                    strong: "b".into(),
                    cheap_fallback: None,
                    strong_fallback: None,
                },
                [false, false, false, false],
            ),
            (
                "persist_default_model",
                Payload::PersistDefaultModel {
                    model: "m".into(),
                    provider: "anthropic".into(),
                },
                [false, false, false, false],
            ),
            (
                "router_defaults_req",
                Payload::RouterDefaultsReq {},
                [false, true, false, false],
            ),
            (
                "models_req",
                Payload::ModelsReq {},
                [false, true, false, false],
            ),
            (
                "personas_req",
                Payload::PersonasReq {},
                [false, true, false, false],
            ),
        ]
    }

    /// Every client-to-server frame name the protocol has, and therefore every
    /// name the table above must cover.
    const CLIENT_FRAME_NAMES: &[&str] = &[
        "hello",
        "attach",
        "detach",
        "submit",
        "interrupt",
        "shell",
        "steer_agent",
        "kill_agent",
        "steer",
        "enqueue",
        "dequeue",
        "compact",
        "wake",
        "cancel_wake",
        "approve",
        "decide_plan",
        "set_config",
        "explain_rule",
        "fork",
        "new_session",
        "list_sessions",
        "delete_session",
        "create_task",
        "get_task_status",
        "merge_task",
        "automations_list",
        "automation_add",
        "automation_remove",
        "remote_control",
        "publish_page",
        "auth_providers",
        "auth_start",
        "auth_set_key",
        "auth_remove",
        "persist_router",
        "ping",
        "router_defaults_req",
        "models_req",
        "personas_req",
        "persist_default_model",
    ];

    /// The persona summary is the first line of the prompt, display-safe
    /// (no bidi or control characters) and capped at 80 characters.
    #[test]
    fn persona_summary_is_the_first_line_sanitized_and_capped() {
        assert_eq!(
            persona_summary(Some("  You review diffs.\nSecond line.")),
            "You review diffs."
        );
        assert_eq!(persona_summary(Some("a\u{202E}b")), "a?b");
        assert_eq!(persona_summary(None), "");
        assert_eq!(persona_summary(Some("")), "");
        let long = "x".repeat(200);
        assert_eq!(persona_summary(Some(&long)).chars().count(), 80);
    }

    /// Rows come out sorted by name with their model and summary, and the
    /// full system prompt never reaches the wire.
    #[test]
    fn persona_rows_are_sorted_and_never_carry_the_prompt() {
        use modron_core::PersonaSpec;
        let mut personas = std::collections::BTreeMap::new();
        let spec = |system: &str, model: Option<&str>| PersonaSpec {
            system: Some(system.to_string()),
            model: model.map(String::from),
            provider: None,
            allow: Vec::new(),
            deny: Vec::new(),
        };
        personas.insert(
            "writer".to_string(),
            spec("Write prose.\nSECRET-BODY", None),
        );
        personas.insert(
            "reviewer".to_string(),
            spec("Review code.\nSECRET-BODY", Some("claude-haiku-4-5")),
        );
        let rows = persona_rows(&personas);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "reviewer");
        assert_eq!(rows[0].model.as_deref(), Some("claude-haiku-4-5"));
        assert_eq!(rows[0].summary, "Review code.");
        assert_eq!(rows[1].name, "writer");
        assert_eq!(rows[1].model, None);
        let wire = serde_json::to_string(&rows).unwrap();
        assert!(!wire.contains("SECRET-BODY"), "the prompt leaked: {wire}");
    }

    #[test]
    fn relay_table_covers_every_client_variant() {
        let own = SessionId::new_v4();
        let other = SessionId::new_v4();
        let rows = relay_rows(own, other);
        let mut seen: Vec<&str> = rows.iter().map(|(_, p, _)| variant_name(p)).collect();
        seen.sort();
        seen.dedup();
        let mut expected: Vec<&str> = CLIENT_FRAME_NAMES.to_vec();
        expected.sort();
        assert_eq!(seen, expected, "every client frame needs a row");
    }

    #[test]
    fn relay_gate_decides_every_row_for_every_role() {
        use modron_protocol::RelayRole::*;
        let own = SessionId::new_v4();
        let other = SessionId::new_v4();
        for (label, payload, [legacy, owner, approver, viewer]) in relay_rows(own, other) {
            assert_eq!(
                scoped_gate(own, &payload).is_ok(),
                legacy,
                "legacy scope gate: {label}"
            );
            assert!(
                relay_gate(own, None, &payload).is_err(),
                "a relayed frame without a role is refused: {label}"
            );
            for (role, want) in [(Owner, owner), (Approver, approver), (Viewer, viewer)] {
                assert_eq!(
                    relay_gate(own, Some(role), &payload).is_ok(),
                    want,
                    "trusted {role:?}: {label}"
                );
            }
        }
    }

    /// A local connection ignores `role`: a viewer-tagged frame from a local
    /// socket still runs the server-wide request it names.
    #[tokio::test]
    async fn local_connection_ignores_relayed_role() {
        let (state, _dir) = test_state();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut local = ConnCtx::new(tx);
        local.authed = true;
        let mut frame = Frame::request(3, Payload::ListSessions {});
        frame.role = Some(modron_protocol::RelayRole::Viewer);
        handle_frame(&state, &mut local, false, frame).await;
        match next_outbound(&mut rx).await {
            Outbound::Reply(frame) => assert!(
                matches!(frame.payload, Payload::Sessions { .. }),
                "{:?}",
                frame.payload
            ),
            other => panic!("expected a reply, got {other:?}"),
        }
    }

    /// A tunnel without the host token is the legacy share-link relay: a
    /// frame claiming `role: owner` gains nothing, and shell stays refused.
    #[tokio::test]
    async fn untrusted_tunnel_frame_claiming_owner_gains_nothing() {
        let (state, _dir) = test_state();
        let id = SessionId::new_v4();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut link = ConnCtx::scoped(tx, id);
        for payload in [
            Payload::SetConfig {
                patch: Default::default(),
            },
            Payload::Shell {
                session_id: id,
                command: "ls".into(),
                private: false,
            },
        ] {
            let mut frame = Frame::request(4, payload);
            frame.role = Some(modron_protocol::RelayRole::Owner);
            handle_relayed_frame(&state, &mut link, frame).await;
            match next_outbound(&mut rx).await {
                Outbound::Reply(frame) => match frame.payload {
                    Payload::Error { kind, .. } => {
                        assert_eq!(kind, Some(modron_protocol::ErrorKind::Forbidden))
                    }
                    other => panic!("expected an error, got {other:?}"),
                },
                other => panic!("expected a reply, got {other:?}"),
            }
        }
    }

    /// AuthRemove is local-only: a trusted tunnel refuses it for every
    /// relayed role, and the env file is left as it was.
    #[tokio::test]
    async fn auth_remove_is_refused_over_a_tunnel_for_every_role() {
        let (state, dir) = test_state();
        let env = dir.path().join("env");
        std::fs::write(&env, "export GROQ_API_KEY=\"gsk_tunnel_fixture\"\n").unwrap();
        let id = SessionId::new_v4();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut tunnel = ConnCtx::trusted_tunnel(tx, id);
        tunnel.authed = true;
        for role in [
            modron_protocol::RelayRole::Owner,
            modron_protocol::RelayRole::Approver,
            modron_protocol::RelayRole::Viewer,
        ] {
            let mut frame = Frame::request(
                20,
                Payload::AuthRemove {
                    session_id: id,
                    provider: "groq".into(),
                },
            );
            frame.role = Some(role);
            handle_relayed_frame(&state, &mut tunnel, frame).await;
            match next_outbound(&mut rx).await {
                Outbound::Reply(frame) => match frame.payload {
                    Payload::Error { kind, .. } => assert_eq!(
                        kind,
                        Some(modron_protocol::ErrorKind::Forbidden),
                        "{role:?} is refused"
                    ),
                    _ => panic!("{role:?}: expected a forbidden error"),
                },
                other => panic!("{role:?}: expected a reply, got {other:?}"),
            }
        }
        assert!(
            std::fs::read_to_string(&env)
                .unwrap()
                .contains("GROQ_API_KEY"),
            "a refused tunnel frame changes nothing"
        );
    }

    /// Serialises the tests that point HOME and MODRON_CONFIG at a temp dir.
    static CREDENTIAL_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Local AuthRemove: the key line goes, the note carries no secret, and
    /// the refreshed `auth_providers` rows show "not configured". Unknown
    /// providers get a NotFound error.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // serialises env mutation; single-threaded runtime
    async fn auth_remove_local_updates_the_rows() {
        let _guard = CREDENTIAL_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let (state, _dir) = test_state();
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join("config");
        std::fs::create_dir_all(&config).unwrap();
        let env = config.join("env");
        let fixture = "gsk_local_fixture_value";
        std::fs::write(
            &env,
            format!("export GROQ_API_KEY=\"{fixture}\"\nexport ZAI_API_KEY=\"zai_keep\"\n"),
        )
        .unwrap();
        let saved_home = std::env::var_os("HOME");
        let saved_config = std::env::var_os("MODRON_CONFIG");
        let saved_key = std::env::var_os("GROQ_API_KEY");
        std::env::set_var("HOME", home.path());
        std::env::set_var("MODRON_CONFIG", &config);
        std::env::set_var("GROQ_API_KEY", fixture);

        let id = SessionId::new_v4();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut local = ConnCtx::new(tx);
        local.authed = true;
        let groq_row = |frame: &Frame| match &frame.payload {
            Payload::AuthProviders { rows } => rows
                .iter()
                .find(|r| r.id == "groq")
                .map(|r| r.status.clone()),
            _ => None,
        };
        let frame = Frame::request(
            30,
            Payload::AuthRemove {
                session_id: id,
                provider: "groq".into(),
            },
        );
        handle_frame(&state, &mut local, true, frame).await;
        let note = match next_outbound(&mut rx).await {
            Outbound::Reply(frame) => frame,
            other => panic!("expected the note reply, got {other:?}"),
        };
        assert_eq!(note.id, Some(30), "the note answers the request");
        let text = match &note.payload {
            Payload::Note { text } => text.clone(),
            Payload::Error { message, .. } => panic!("expected a note, got error: {message}"),
            _ => panic!("expected a note"),
        };
        assert!(text.contains("API key removed"), "note names the removal");
        assert!(!text.contains(fixture), "note carries no key");
        let rows = match next_outbound(&mut rx).await {
            Outbound::Reply(frame) => frame,
            other => panic!("expected the rows, got {other:?}"),
        };
        assert_eq!(
            groq_row(&rows).as_deref(),
            Some("not configured"),
            "the groq row returns to not configured"
        );
        let after = std::fs::read_to_string(&env).unwrap();
        assert!(!after.contains("GROQ_API_KEY"), "groq line removed");
        assert!(after.contains("zai_keep"), "other provider line kept");

        let bad = Frame::request(
            31,
            Payload::AuthRemove {
                session_id: id,
                provider: "no-such-provider".into(),
            },
        );
        handle_frame(&state, &mut local, true, bad).await;
        match next_outbound(&mut rx).await {
            Outbound::Reply(frame) => match frame.payload {
                Payload::Error { kind, .. } => {
                    assert_eq!(kind, Some(modron_protocol::ErrorKind::NotFound))
                }
                _ => panic!("expected an error for an unknown provider"),
            },
            other => panic!("expected a reply, got {other:?}"),
        }

        let restore = |name: &str, value: Option<std::ffi::OsString>| match value {
            Some(v) => std::env::set_var(name, v),
            None => std::env::remove_var(name),
        };
        restore("HOME", saved_home);
        restore("MODRON_CONFIG", saved_config);
        restore("GROQ_API_KEY", saved_key);
    }

    /// On a trusted tunnel the relayed role decides: a viewer may ping but
    /// not configure, an approver may not run a shell, the owner passes the
    /// gate and reaches the session (NotFound: the session does not exist).
    #[tokio::test]
    async fn trusted_tunnel_applies_the_relayed_role() {
        let (state, _dir) = test_state();
        let id = SessionId::new_v4();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut tunnel = ConnCtx::trusted_tunnel(tx, id);
        tunnel.authed = true;

        let mut ping = Frame::request(10, Payload::Ping {});
        ping.role = Some(modron_protocol::RelayRole::Viewer);
        handle_relayed_frame(&state, &mut tunnel, ping).await;
        match next_outbound(&mut rx).await {
            Outbound::Reply(frame) => assert!(matches!(frame.payload, Payload::Pong {})),
            other => panic!("expected a reply, got {other:?}"),
        }

        let kind_of = |outbound: Outbound| match outbound {
            Outbound::Reply(frame) => match frame.payload {
                Payload::Error { kind, .. } => kind,
                _ => None,
            },
            _ => None,
        };
        for role in [
            modron_protocol::RelayRole::Approver,
            modron_protocol::RelayRole::Viewer,
        ] {
            let mut shell = Frame::request(
                11,
                Payload::Shell {
                    session_id: id,
                    command: "ls".into(),
                    private: false,
                },
            );
            shell.role = Some(role);
            handle_relayed_frame(&state, &mut tunnel, shell).await;
            assert_eq!(
                kind_of(next_outbound(&mut rx).await),
                Some(modron_protocol::ErrorKind::Forbidden),
                "{role:?} shell"
            );
        }
        let mut owner_shell = Frame::request(
            12,
            Payload::Shell {
                session_id: id,
                command: "ls".into(),
                private: false,
            },
        );
        owner_shell.role = Some(modron_protocol::RelayRole::Owner);
        handle_relayed_frame(&state, &mut tunnel, owner_shell).await;
        assert_eq!(
            kind_of(next_outbound(&mut rx).await),
            Some(modron_protocol::ErrorKind::NotFound),
            "owner shell passes the gate"
        );
        // A relayed frame with no role on a trusted tunnel is refused.
        handle_relayed_frame(&state, &mut tunnel, Frame::request(13, Payload::Ping {})).await;
        assert_eq!(
            kind_of(next_outbound(&mut rx).await),
            Some(modron_protocol::ErrorKind::Forbidden)
        );
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

    /// PARITY item 5: `Shell` is refused for a missing session, an empty
    /// command, a busy session, and (until the engine entry exists) an idle
    /// one. A share-link connection cannot send it at all.
    #[tokio::test]
    async fn shell_passthrough_refusal_paths() {
        let (state, _dir) = test_state();
        let id = state.store.create_session("shell").await.unwrap().id;
        state.hub.ensure_session(id, None);
        let (mut ctx, mut rx) = authed_ctx();
        subscribe_first(&state, &mut ctx, &mut rx, id).await;

        let run = |command: &str| Payload::Shell {
            session_id: id,
            command: command.into(),
            private: false,
        };
        handle_frame(&state, &mut ctx, true, Frame::request(1, run("   "))).await;
        assert!(error_message(next_outbound(&mut rx).await).contains("usage: !"));

        handle_frame(
            &state,
            &mut ctx,
            true,
            Frame::request(
                2,
                Payload::Shell {
                    session_id: SessionId::new_v4(),
                    command: "ls".into(),
                    private: false,
                },
            ),
        )
        .await;
        assert!(error_message(next_outbound(&mut rx).await).contains("no such session"));

        // Idle and no bash tool registered: the run starts and refuses the
        // command with a reason, ending with TurnDone (no Notify).
        handle_frame(&state, &mut ctx, true, Frame::request(3, run("ls"))).await;
        let types = collect_live_types_until(&mut rx, "turn_done").await;
        assert!(types.contains(&"note".to_string()), "{types:?}");
        assert!(!types.contains(&"notify".to_string()), "{types:?}");
        assert!(!state.hub.handle(&id).unwrap().is_busy());

        state.hub.start_run(id, parked_run(id)).unwrap();
        handle_frame(&state, &mut ctx, true, Frame::request(4, run("ls"))).await;
        // Skip any attach snapshot still queued on this subscription.
        let busy = loop {
            match next_outbound(&mut rx).await {
                Outbound::Reply(frame) if matches!(frame.payload, Payload::Error { .. }) => {
                    break error_message(Outbound::Reply(frame));
                }
                _ => {}
            }
        };
        assert!(busy.contains("run is active"), "{busy}");
        state.hub.handle(&id).unwrap().interrupt();

        // A share link is scoped to its session and may not run commands.
        let (tx, mut link_rx) = mpsc::unbounded_channel();
        let mut link = ConnCtx::scoped(tx, id);
        handle_frame(&state, &mut link, false, Frame::request(5, run("ls"))).await;
        match next_outbound(&mut link_rx).await {
            Outbound::Reply(frame) => match frame.payload {
                Payload::Error { kind, .. } => {
                    assert_eq!(kind, Some(modron_protocol::ErrorKind::Forbidden))
                }
                other => panic!("expected an error, got {other:?}"),
            },
            other => panic!("expected a reply, got {other:?}"),
        }
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
