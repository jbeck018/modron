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
    /// The repo the server serves (git worktree tasks run here).
    pub working_dir: PathBuf,
    /// Worktree-task registry: session id -> task record.
    pub tasks: crate::tasks::TaskRegistry,
    /// Configured remote-control bridge base (scheme + host), if any.
    pub bridge_base: Option<String>,
    /// Session -> share key (minted by /remote-control; used to auth
    /// page publishes).
    pub rc_keys: std::sync::Mutex<HashMap<SessionId, String>>,
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

/// Per-connection state.
pub struct ConnCtx {
    /// Server->client frames (the transport's writer task drains it).
    pub out: mpsc::UnboundedSender<Outbound>,
    /// Set by the `hello` handshake.
    pub authed: bool,
    pub subscription: Option<Subscription>,
}

impl ConnCtx {
    pub fn new(out: mpsc::UnboundedSender<Outbound>) -> Self {
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
    let _ = ctx.out.send(Outbound::Reply(Box::new(frame)));
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
                    if !handle.interrupt() {
                        send(ctx, error(request_id, "no active run in this session"));
                    }
                    // Success announces itself as TurnAborted.
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

async fn handle_router_defaults_req(
    state: &Arc<ServerState>,
    ctx: &mut ConnCtx,
    request_id: Option<u64>,
) {
    // What the settings page's defaults card prefills: the authed
    // catalog providers, the derived recommendation (the same
    // auto-routing the serve startup notes), and the fallback
    // chains that follow.
    let authed: Vec<String> = modron_core::auth::provider_registry()
        .iter()
        .filter(|p| modron_core::auth::catalog_provider_authenticated(p.provider_id))
        .map(|p| p.provider_id.to_string())
        .collect();
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

async fn handle_delete_session(
    state: &Arc<ServerState>,
    ctx: &mut ConnCtx,
    request_id: Option<u64>,
    session_id: SessionId,
) {
    // A running session keeps its history until it settles.
    if state
        .hub
        .handle(&session_id)
        .is_some_and(|handle| handle.is_busy())
    {
        send(
            ctx,
            error(
                request_id,
                "session is running - interrupt it before deleting",
            ),
        );
        return;
    }
    match state.store.delete(session_id).await {
        Ok(true) => {
            state.hub.remove(&session_id);
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
        }
        Ok(false) => {
            send(
                ctx,
                error_kind(
                    request_id,
                    format!("no such session {session_id}"),
                    modron_protocol::ErrorKind::NotFound,
                ),
            );
        }
        Err(e) => send(ctx, error(request_id, format!("delete failed: {e}"))),
    }
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
    state: &Arc<ServerState>,
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
    let (buf_tx, buf_rx) = mpsc::unbounded_channel::<Outbound>();
    let forward = tokio::spawn(async move {
        let mut rx = rx;
        loop {
            match rx.recv().await {
                Ok(live) => {
                    if buf_tx.send(Outbound::Live(Box::new(live))).is_err() {
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
    if ctx.out.send(Outbound::Reply(Box::new(snapshot))).is_err() {
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
        Payload::Approve { .. } => "approve",
        Payload::SetConfig { .. } => "set_config",
        Payload::Fork { .. } => "fork",
        Payload::NewSession { .. } => "new_session",
        Payload::ListSessions {} => "list_sessions",
        Payload::DeleteSession { .. } => "delete_session",
        Payload::AuthStart { .. } => "auth_start",
        Payload::RouterDefaultsReq { .. } => "router_defaults_req",
        Payload::RouterDefaults { .. } => "router_defaults",
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
