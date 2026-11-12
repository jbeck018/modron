//! `RemoteClient` — the mobile remote-control client: one WebSocket to
//! the modron server, one background task folding every inbound [`Frame`]
//! into a [`SessionViewModel`] behind a std mutex, and a polling API for
//! UI shells (`state()` / `take_new_entries()` / `take_pending_approval()`
//! plus one method per client command). No callbacks: a shell ticks the
//! snapshot on its render loop.
//!
//! ```ignore
//! let client = RemoteClient::connect(&url, None)?; // fire-and-forget
//! // render loop polls:
//! let snap = client.state();                     // cheap clone, serde-ready
//! for entry in client.take_new_entries() { ... } // incremental rows
//! client.submit(Some(session_id), "fix the bug")?;
//! ```
//!
//! `connect` never blocks the caller: it flips the state to `Connecting`
//! and completes the handshake + run loop on a background task (tokio on
//! native, `spawn_local` on wasm — a browser main thread cannot block).
//! Errors land in `state().last_error`. Reconnect is explicit
//! (`reconnect`), resuming from the last seen per-session `seq` so the
//! server replays exactly the missed events.

use crate::error::ClientError;
use crate::transport;
use crate::view::{
    fold_snapshot, fold_turn_done, fold_turn_unfinished, FoldState, OAuthProgressView,
    PendingApprovalView, SessionViewModel, ViewModelSnapshot,
};
use modron_protocol::{
    ConfigPatch, Decision, Frame, Payload, WireDelta, DEFAULT_PORT, PROTOCOL_VERSION,
};
use modron_types::ToolActivity;
use modron_view::TranscriptEntry;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

/// The `client` string sent in the hello frame.
pub const CLIENT_NAME: &str = "modron-mobile";

/// Loopback URL for the default remote port (TLS terminates in front of
/// the server; the client speaks wss:// in production).
pub fn default_url() -> String {
    format!("ws://127.0.0.1:{DEFAULT_PORT}")
}

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Inbound events after the handshake; the transport-specific shapes
/// (tungstenite messages / browser events) normalize to these.
#[allow(dead_code)] // `Open` is wasm-only (browser onopen)
pub(crate) enum In {
    /// Browser sockets only: `onopen` fired (the handshake uses it).
    Open,
    /// A text frame (the wire speaks JSON).
    Text(String),
    /// The socket closed (cleanly or not).
    Closed,
}

/// Reject URLs a browser `WebSocket` constructor would throw on
/// (a JS exception inside wasm aborts the whole module — validate first).
fn validate_ws_url(url: &str) -> Result<(), ClientError> {
    let after_scheme = url
        .strip_prefix("ws://")
        .or_else(|| url.strip_prefix("wss://"))
        .ok_or_else(|| ClientError::Connect("url must start with ws:// or wss://".into()))?;
    let host = after_scheme.split(['/', '?']).next().unwrap_or("");
    if host.is_empty() {
        return Err(ClientError::Connect("url has no host".into()));
    }
    Ok(())
}

/// Runtime-agnostic deadline wrapper. Native: futures-timer polls a
/// deadline against `Instant`. Wasm: `Instant` is unimplemented on
/// wasm32-unknown-unknown (panics), so the delay is a browser `setTimeout`
/// resolving a oneshot; the waiter races it with `select`.
pub(crate) async fn with_timeout<F: Future>(dur: Duration, fut: F) -> Option<F::Output> {
    use futures_util::future::{select, Either};
    let mut fut = Box::pin(fut);
    let mut delay = Box::pin(delay(dur));
    match select(&mut fut, &mut delay).await {
        Either::Left((out, _)) => Some(out),
        Either::Right(_) => None,
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) async fn delay(dur: Duration) {
    futures_timer::Delay::new(dur).await;
}

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::JsCast;

#[cfg(target_arch = "wasm32")]
pub(crate) async fn delay(dur: Duration) {
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let cb = wasm_bindgen::closure::Closure::once(move || {
        let _ = tx.send(());
    });
    let window = web_sys::window().expect("browser window");
    window
        .set_timeout_with_callback_and_timeout_and_arguments_0(
            cb.as_ref().unchecked_ref(),
            dur.as_millis() as i32,
        )
        .expect("setTimeout");
    // `Closure::once` releases itself when the timer fires; a never-fired
    // timer (page unload) leaks the box, which is fine at that point.
    std::mem::forget(cb);
    let _ = rx.await;
}
pub(crate) fn handshake_timeout() -> Duration {
    HANDSHAKE_TIMEOUT
}

/// The uncorrelated hello frame (id 0 is reserved for it).
pub(crate) fn hello_frame(token: Option<&str>) -> Frame {
    Frame::request(
        0,
        Payload::Hello {
            client: CLIENT_NAME.into(),
            protocol: PROTOCOL_VERSION,
            token: token.map(Into::into),
        },
    )
}

// ---------------------------------------------------------------------------
// Background task driver
//
// Native: the client owns its runtime (the shell's thread has no tokio
// context). Wasm: `spawn_local` runs the task on the JS microtask queue,
// woken by socket callbacks and channel sends.
// ---------------------------------------------------------------------------

#[cfg(not(target_arch = "wasm32"))]
static RT: std::sync::LazyLock<tokio::runtime::Runtime> = std::sync::LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime for the remote client")
});

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn spawn_task(fut: impl Future<Output = ()> + Send + 'static) {
    RT.spawn(fut);
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn spawn_task(fut: impl Future<Output = ()> + 'static) {
    wasm_bindgen_futures::spawn_local(fut);
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

/// Everything the run loop and the UI-facing methods share.
struct Shared {
    /// View model + fold bookkeeping.
    state: Mutex<CoreState>,
    /// Current connection's outgoing frame channel; `None` when down.
    cmd: Mutex<Option<mpsc::UnboundedSender<Frame>>>,
    /// Flips to true once the connection dies (`wait_closed`).
    closed_tx: watch::Sender<bool>,
    closed_rx: watch::Receiver<bool>,
    /// Connection generation: a stale loop's shutdown must not clobber a
    /// newer connection's state (reconnect while an old loop is still
    /// draining).
    gen: AtomicU64,
    /// Request-id counter (0 is reserved for the uncorrelated hello).
    next_id: AtomicU64,
}

#[derive(Default)]
struct CoreState {
    vm: SessionViewModel,
    fold: FoldState,
}

impl Shared {
    fn new() -> Self {
        let (closed_tx, closed_rx) = watch::channel(false);
        Self {
            state: Mutex::new(CoreState::default()),
            cmd: Mutex::new(None),
            closed_tx,
            closed_rx,
            gen: AtomicU64::new(0),
            next_id: AtomicU64::new(0),
        }
    }

    fn next_request_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::SeqCst) + 1
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// A handle to the remote connection + folded view state. Cloneable; all
/// clones share one connection and one view.
#[derive(Clone)]
pub struct RemoteClient {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for RemoteClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let st = self.shared.state.lock().unwrap();
        f.debug_struct("RemoteClient")
            .field("connection", &st.vm.connection)
            .field("session_id", &st.vm.session_id)
            .field("last_seq", &st.vm.last_seq)
            .finish()
    }
}

impl RemoteClient {
    /// Validate the URL, mark `Connecting`, and start the handshake +
    /// run loop on a background task. Errors land in `state().last_error`
    /// (and `connection` goes `Disconnected`); only a malformed URL
    /// fails synchronously.
    pub fn connect(url: &str, token: Option<&str>) -> Result<Self, ClientError> {
        validate_ws_url(url)?;
        let client = Self {
            shared: Arc::new(Shared::new()),
        };
        client.spin_up(url, token, None);
        Ok(client)
    }

    /// Reconnect and re-`attach` from the last seen seq: the server
    /// replays every event the client missed while offline. The attach
    /// goes out right after the welcome (inside the same background
    /// task), so the caller just watches the state.
    pub fn reconnect(
        &self,
        url: &str,
        session_id: Uuid,
        token: Option<&str>,
    ) -> Result<(), ClientError> {
        validate_ws_url(url)?;
        let last_seq = {
            let st = self.shared.state.lock().unwrap();
            st.vm.last_seq
        };
        self.spin_up(url, token, Some((session_id, last_seq)));
        Ok(())
    }

    /// Mark connecting, clear stale state, and spawn the establish task.
    /// Bumps the generation so a still-draining old loop cannot clobber
    /// state. `resume` is the reconnect's post-handshake attach.
    fn spin_up(&self, url: &str, token: Option<&str>, resume: Option<(Uuid, u64)>) {
        {
            let mut st = self.shared.state.lock().unwrap();
            st.vm.connection = crate::view::ConnectionState::Connecting;
            st.vm.last_error = None;
            st.vm.sessions.clear();
        }
        let gen = self.shared.gen.fetch_add(1, Ordering::SeqCst) + 1;
        let shared = self.shared.clone();
        let url = url.to_string();
        let token = token.map(str::to_string);
        spawn_task(establish(shared, gen, url, token, resume));
    }

    // -- polling API ------------------------------------------------------

    /// Cheap clone of the view state plus every pre-rendered string a
    /// shell displays verbatim.
    pub fn state(&self) -> ViewModelSnapshot {
        let st = self.shared.state.lock().unwrap();
        ViewModelSnapshot::from_vm(&st.vm)
    }

    /// Drain transcript rows committed since the last call (polling UIs
    /// append these instead of re-rendering `state().history`).
    pub fn take_new_entries(&self) -> Vec<TranscriptEntry> {
        let mut st = self.shared.state.lock().unwrap();
        std::mem::take(&mut st.vm.new_entries)
    }

    /// Take the pending approval out of the view (the wire truth stays
    /// authoritative: the server's `ApprovalResolved` echo clears the
    /// rest of the state). Non-destructive reads use
    /// `state().pending_approval`.
    pub fn take_pending_approval(&self) -> Option<PendingApprovalView> {
        let mut st = self.shared.state.lock().unwrap();
        st.vm.pending_approval.take()
    }

    /// Last seen server seq for the attached session (reconnect resumes
    /// from here).
    pub fn last_seq(&self) -> u64 {
        self.shared.state.lock().unwrap().vm.last_seq
    }

    pub fn is_connected(&self) -> bool {
        self.shared.state.lock().unwrap().vm.connection == crate::view::ConnectionState::Connected
    }

    /// Opt in to streamed reasoning (AGENTS.md invariant 5: reasoning
    /// streams only when the user opted in; the TUI's `/reasoning` and
    /// the desktop's reasoning chip default off). While off, reasoning
    /// deltas are dropped in the fold and never reach the snapshot.
    /// Turning it off clears the live buffer, like the TUI's toggle.
    pub fn set_show_reasoning(&self, enable: bool) {
        let mut st = self.shared.state.lock().unwrap();
        st.vm.show_reasoning = enable;
        if !enable {
            st.vm.streaming_reasoning.clear();
        }
    }

    /// Resolves when the connection dies (server close, network loss).
    /// Errors (sender dropped with the last client clone) count as closed.
    pub async fn wait_closed(&self) {
        let mut rx = self.shared.closed_rx.clone();
        let _ = rx.wait_for(|closed| *closed).await;
    }

    // -- commands ---------------------------------------------------------

    /// Subscribe to a session's events; the `Snapshot` folds into the
    /// view in the background (poll `state()` / `take_new_entries()`).
    /// `last_seq = 0` asks for the full replay.
    pub fn attach(&self, session_id: Uuid, last_seq: u64) -> Result<(), ClientError> {
        self.send_frame(Frame::request(
            self.next_request_id(),
            Payload::Attach {
                session_id,
                last_seq,
            },
        ))
    }

    /// Stop receiving the session's events (the session keeps running).
    pub fn detach(&self) -> Result<(), ClientError> {
        self.send_frame(Frame::request(self.next_request_id(), Payload::Detach {}))
    }

    /// Submit a task; `None` starts a new session (the server echoes the
    /// assigned id on subsequent frames).
    pub fn submit(
        &self,
        session_id: Option<Uuid>,
        task: impl Into<String>,
    ) -> Result<(), ClientError> {
        self.send_frame(Frame::request(
            self.next_request_id(),
            Payload::Submit {
                session_id,
                task: task.into(),
                attachments: Vec::new(),
            },
        ))
    }

    /// Abort the session's active run (TUI Esc parity).
    pub fn interrupt(&self, session_id: Uuid) -> Result<(), ClientError> {
        self.send_frame(Frame::request(
            self.next_request_id(),
            Payload::Interrupt { session_id },
        ))
    }

    /// Answer a pending approval: the three shared decisions
    /// (`allow once` / `allow all` / `deny`); allow-all is
    /// sticky for the rest of the run, resolved server-side.
    pub fn approve(
        &self,
        request_id: impl Into<String>,
        decision: Decision,
    ) -> Result<(), ClientError> {
        self.send_frame(Frame::request(
            self.next_request_id(),
            Payload::Approve {
                request_id: request_id.into(),
                decision,
            },
        ))
    }

    /// Mutate config for future turns only (server semantics).
    pub fn set_config(&self, patch: ConfigPatch) -> Result<(), ClientError> {
        self.send_frame(Frame::request(
            self.next_request_id(),
            Payload::SetConfig { patch },
        ))
    }

    /// Model switch: send the model only. The server follows the provider
    /// through the catalog (hub.apply_config); pinning the CURRENT provider
    /// here would cross-pin a foreign model to the old provider
    /// (2026-11-12 audit fix - desktop parity via modron_router::lookup).
    pub fn select_model(&self, model: impl Into<String>) -> Result<(), ClientError> {
        self.set_config(ConfigPatch {
            model: Some(model.into()),
            ..ConfigPatch::default()
        })
    }

    /// List sessions: fires the request; the rows land in
    /// `state().sessions` (polling UIs read them there).
    pub fn list_sessions(&self) -> Result<(), ClientError> {
        self.send_frame(Frame::request(
            self.next_request_id(),
            Payload::ListSessions {},
        ))
    }

    /// List automations: fires the request; the rows land in
    /// `state().automations` (settings views read them there).
    pub fn list_automations(&self) -> Result<(), ClientError> {
        self.send_frame(Frame::request(
            self.next_request_id(),
            Payload::AutomationsList {},
        ))
    }

    /// Add an automation (a prompt on an interval); the refreshed rows
    /// land in `state().automations`.
    pub fn add_automation(
        &self,
        name: impl Into<String>,
        prompt: impl Into<String>,
        interval_minutes: u32,
    ) -> Result<(), ClientError> {
        self.send_frame(Frame::request(
            self.next_request_id(),
            Payload::AutomationAdd {
                name: name.into(),
                prompt: prompt.into(),
                interval_minutes,
            },
        ))
    }

    /// Remove an automation by name; the refreshed rows land in
    /// `state().automations`.
    pub fn remove_automation(&self, name: impl Into<String>) -> Result<(), ClientError> {
        self.send_frame(Frame::request(
            self.next_request_id(),
            Payload::AutomationRemove { name: name.into() },
        ))
    }

    /// Publish the transcript as a read-only page on the bridge; the URL
    /// lands in `state().last_note`.
    pub fn publish_page(
        &self,
        session_id: Uuid,
        title: impl Into<String>,
        content_md: impl Into<String>,
    ) -> Result<(), ClientError> {
        self.send_frame(Frame::request(
            self.next_request_id(),
            Payload::PublishPage {
                session_id,
                title: title.into(),
                content_md: content_md.into(),
            },
        ))
    }

    /// Open a remote-control tunnel for the session; the share URL lands
    /// in `state().last_note` (the server notes it too).
    pub fn remote_control(&self, session_id: Uuid, bridge_url: &str) -> Result<(), ClientError> {
        self.send_frame(Frame::request(
            self.next_request_id(),
            Payload::RemoteControl {
                session_id,
                bridge_url: bridge_url.into(),
            },
        ))
    }

    /// Delete a session on the server. The server replies with a note
    /// (or an error when the session is running); refresh the list with
    /// [`Self::list_sessions`] after the delete settles.
    pub fn delete_session(&self, session_id: Uuid) -> Result<(), ClientError> {
        self.send_frame(Frame::request(
            self.next_request_id(),
            Payload::DeleteSession { session_id },
        ))
    }

    /// Request the provider auth rows; the reply lands in
    /// `state().providers` (the same payload doubles as request with
    /// empty rows and reply, matching the desktop client).
    pub fn auth_providers(&self) -> Result<(), ClientError> {
        self.send_frame(Frame::request(
            self.next_request_id(),
            Payload::AuthProvidersReq {},
        ))
    }

    /// Start a provider's sign-in flow on the server; progress streams
    /// into `state().oauth` and the outcome arrives as a note.
    pub fn auth_start(&self, session_id: Uuid, provider: &str) -> Result<(), ClientError> {
        self.send_frame(Frame::request(
            self.next_request_id(),
            Payload::AuthStart {
                session_id,
                provider: provider.into(),
            },
        ))
    }

    /// Save an API key for a provider; the outcome arrives as a note.
    pub fn auth_set_key(
        &self,
        session_id: Uuid,
        provider: &str,
        key: &str,
    ) -> Result<(), ClientError> {
        self.send_frame(Frame::request(
            self.next_request_id(),
            Payload::AuthSetKey {
                session_id,
                provider: provider.into(),
                key: key.into(),
            },
        ))
    }

    /// Create an empty session without submitting a task.
    pub fn new_session(&self, title: Option<&str>) -> Result<(), ClientError> {
        self.send_frame(Frame::request(
            self.next_request_id(),
            Payload::NewSession {
                title: title.map(Into::into),
            },
        ))
    }

    /// Fork a session: the server copies its log under a fresh id and
    /// both sessions get a note.
    pub fn fork_session(&self, session_id: Uuid) -> Result<(), ClientError> {
        self.send_frame(Frame::request(
            self.next_request_id(),
            Payload::Fork { session_id },
        ))
    }

    /// Create a task: a new session in a dedicated worktree/branch off
    /// `base` (the reply adopts the new session).
    pub fn create_task(&self, title: &str, base: Option<&str>) -> Result<(), ClientError> {
        self.send_frame(Frame::request(
            self.next_request_id(),
            Payload::CreateTask {
                title: title.into(),
                base: base.map(Into::into),
            },
        ))
    }

    /// Request a task's diff/branch status; the answer lands in
    /// `state().last_note`.
    pub fn task_status(&self, session_id: Uuid) -> Result<(), ClientError> {
        self.send_frame(Frame::request(
            self.next_request_id(),
            Payload::GetTaskStatus { session_id },
        ))
    }

    /// Merge a task's branch back into its base; the answer (ok or the
    /// abort summary) lands in `state().last_note`.
    pub fn merge_task(&self, session_id: Uuid, message: Option<&str>) -> Result<(), ClientError> {
        self.send_frame(Frame::request(
            self.next_request_id(),
            Payload::MergeTask {
                session_id,
                message: message.map(Into::into),
            },
        ))
    }

    /// Request the router's default-model picture; the answer lands in
    /// `state().router_defaults`.
    pub fn router_defaults(&self) -> Result<(), ClientError> {
        self.send_frame(Frame::request(
            self.next_request_id(),
            Payload::RouterDefaultsReq {},
        ))
    }

    /// Persist router defaults into the server's config; the outcome
    /// lands as a note.
    pub fn persist_router(
        &self,
        cheap: &str,
        strong: &str,
        cheap_fallback: Option<&str>,
        strong_fallback: Option<&str>,
    ) -> Result<(), ClientError> {
        self.send_frame(Frame::request(
            self.next_request_id(),
            Payload::PersistRouter {
                cheap: cheap.into(),
                strong: strong.into(),
                cheap_fallback: cheap_fallback.map(Into::into),
                strong_fallback: strong_fallback.map(Into::into),
            },
        ))
    }

    /// Close the connection (the run loop drains, sends a close frame,
    /// and marks the state disconnected).
    pub fn disconnect(&self) {
        *self.shared.cmd.lock().unwrap() = None;
    }

    // -- internals --------------------------------------------------------

    fn next_request_id(&self) -> u64 {
        self.shared.next_request_id()
    }

    fn send_frame(&self, frame: Frame) -> Result<(), ClientError> {
        let guard = self.shared.cmd.lock().unwrap();
        match &*guard {
            Some(tx) => tx.send(frame).map_err(|_| ClientError::Closed),
            None => Err(ClientError::Closed),
        }
    }
}

// ---------------------------------------------------------------------------
// Establish + run loop
// ---------------------------------------------------------------------------

/// The background connection task: handshake, wire the command channel,
/// optionally send the reconnect attach, then run the loop to completion.
async fn establish(
    shared: Arc<Shared>,
    gen: u64,
    url: String,
    token: Option<String>,
    resume: Option<(Uuid, u64)>,
) {
    match transport::handshake(&url, token.as_deref()).await {
        Ok((mut conn, server_version)) => {
            {
                let mut st = shared.state.lock().unwrap();
                st.vm.connection = crate::view::ConnectionState::Connected;
                st.vm.server_version = server_version;
            }
            let (tx, rx) = mpsc::unbounded_channel();
            *shared.cmd.lock().unwrap() = Some(tx.clone());
            if let Some((session_id, last_seq)) = resume {
                let _ = tx.send(Frame::request(
                    shared.next_request_id(),
                    Payload::Attach {
                        session_id,
                        last_seq,
                    },
                ));
            }
            run_loop(&mut conn, shared, gen, rx).await;
        }
        Err(e) => {
            // A stale establish task (reconnect superseded it) must not
            // clobber the newer connection's state.
            if shared.gen.load(Ordering::SeqCst) == gen {
                let mut st = shared.state.lock().unwrap();
                st.vm.connection = crate::view::ConnectionState::Disconnected;
                st.vm.last_error = Some(e.to_string());
            }
        }
    }
}

async fn run_loop(
    conn: &mut transport::Conn,
    shared: Arc<Shared>,
    gen: u64,
    mut rx: mpsc::UnboundedReceiver<Frame>,
) {
    let reason: Option<String> = loop {
        tokio::select! {
            cmd = rx.recv() => match cmd {
                Some(frame) => {
                    let json = match serde_json::to_string(&frame) {
                        Ok(json) => json,
                        Err(e) => {
                            tracing::warn!(error = %e, "outgoing frame encode failed");
                            continue;
                        }
                    };
                    if let Err(e) = conn.send_text(&json).await {
                        break Some(format!("send failed: {e}"));
                    }
                }
                None => {
                    conn.close().await;
                    break None;
                }
            },
            msg = conn.next_in() => match msg {
                Some(Ok(In::Text(json))) => match serde_json::from_str::<Frame>(&json) {
                    Ok(frame) => fold_frame(&shared, frame),
                    Err(e) => tracing::warn!(error = %e, "inbound frame decode failed"),
                },
                Some(Ok(In::Open)) => {}
                Some(Ok(In::Closed)) | None => {
                    break Some("server closed the connection".into());
                }
                Some(Err(e)) => break Some(e.to_string()),
            },
        }
    };
    shutdown(&shared, gen, reason);
}

/// Mark the connection down and release the command channel. Guarded by
/// generation so a stale loop (reconnect in progress) leaves the newer
/// connection's state alone. `None` = clean close (no error surface);
/// `Some` = the cause string.
fn shutdown(shared: &Shared, gen: u64, reason: Option<String>) {
    if shared.gen.load(Ordering::SeqCst) != gen {
        return;
    }
    {
        let mut st = shared.state.lock().unwrap();
        st.vm.connection = crate::view::ConnectionState::Disconnected;
        st.vm.last_error = reason.clone();
    }
    *shared.cmd.lock().unwrap() = None;
    let _ = shared.closed_tx.send(true);
}

// ---------------------------------------------------------------------------
// Frame folding (one defined effect per payload, per modron-protocol docs)
// ---------------------------------------------------------------------------

fn fold_frame(shared: &Shared, frame: Frame) {
    let mut st = shared.state.lock().unwrap();
    let CoreState { vm, fold } = &mut *st;

    // Any session-scoped sequenced frame advances the resume cursor.
    if frame.session.is_some() && frame.seq > vm.last_seq {
        vm.last_seq = frame.seq;
    }

    match frame.payload {
        Payload::Welcome {
            server_version,
            protocol,
        } => {
            if protocol == PROTOCOL_VERSION {
                vm.connection = crate::view::ConnectionState::Connected;
                vm.server_version = server_version;
            }
        }
        Payload::Snapshot {
            session_id,
            last_seq,
            events,
            live,
        } => {
            fold_snapshot(vm, fold, session_id, last_seq, &events, &live);
        }
        Payload::Event { event } => fold.fold_event(vm, event),
        Payload::Delta { delta } => match delta {
            WireDelta::Text { text } => vm.streaming_text.push_str(&text),
            // Parity invariant 5 (reasoning streams only when the user
            // opted in): dropped while `show_reasoning` is off, so the
            // buffer stays empty and no shell can render it. The TUI's
            // `/reasoning` and the desktop's reasoning chip default off.
            WireDelta::Reasoning { text } => {
                if vm.show_reasoning {
                    vm.streaming_reasoning.push_str(&text);
                }
            }
            WireDelta::Done => {}
        },
        Payload::Tool { activity } => {
            let core: ToolActivity = activity.into();
            fold.fold_tool(vm, &core);
        }
        Payload::ApprovalRequested { request_id, call } => {
            vm.pending_approval = Some(PendingApprovalView::new(request_id, &call));
        }
        Payload::ApprovalResolved { request_id, .. } => {
            if vm
                .pending_approval
                .as_ref()
                .is_some_and(|p| p.request_id == request_id)
            {
                vm.pending_approval = None;
            }
        }
        Payload::ConfigChanged { patch } => apply_config(vm, &patch),
        Payload::Sessions { sessions } => vm.sessions = sessions,
        Payload::Automations { rows } => vm.automations = rows,
        Payload::AuthProviders { rows } => vm.providers = rows,
        Payload::RouterDefaults {
            authed,
            recommended_cheap,
            recommended_strong,
            cheap_fallback,
            strong_fallback,
        } => {
            vm.router_defaults = Some(modron_protocol::RouterDefaults {
                authed,
                recommended_cheap,
                recommended_strong,
                cheap_fallback,
                strong_fallback,
            });
        }
        // A task creation adopts the fresh session (the desktop does the
        // same); status and merge answers land as scrollback notes.
        Payload::TaskCreated {
            session_id,
            worktree,
            branch,
        } => {
            vm.session_id = Some(session_id);
            vm.last_note = Some(format!("task created: {branch} in {worktree}"));
        }
        Payload::TaskStatus {
            session_id,
            branch,
            running,
            ahead,
            files,
            ..
        } => {
            vm.last_note = Some(format!(
                "task {session_id}: {branch}, {ahead} ahead, {} files, {}",
                files.len(),
                if running { "running" } else { "idle" }
            ));
        }
        Payload::TaskMerged {
            session_id,
            ok,
            summary,
        } => {
            vm.last_note = Some(if ok {
                format!("merged task {session_id}: {summary}")
            } else {
                format!("merge aborted: {summary}")
            });
        }
        Payload::PagePublished { url } => {
            vm.last_note = Some(format!("published: {url}"));
        }
        // Desktop parity (app_state.rs folds this into last_note too): the
        // share URL lands in the note, which the TSX renders in the chat.
        Payload::RemoteControlStarted { url } => {
            vm.last_note = Some(format!("remote-control live: {url} - open on any device"));
        }
        Payload::TurnDone { summary } => fold_turn_done(vm, fold, &summary),
        Payload::TurnFailed { error } => fold_turn_unfinished(vm, fold, Some(error)),
        Payload::TurnAborted {} => fold_turn_unfinished(vm, fold, None),
        Payload::Note { text } => vm.last_note = Some(text),
        Payload::OAuthProgress {
            provider,
            stage,
            message,
            url,
            user_code,
        } => {
            vm.oauth = Some(OAuthProgressView {
                provider,
                stage,
                message,
                url,
                user_code,
            });
        }
        Payload::Error { message, .. } => {
            vm.last_error = Some(message.clone());
        }
        Payload::Pong {} => {}
        // Client->server payloads never arrive inbound; ignore defensively.
        _ => {}
    }
}

/// Config labels update immediately; the server applies the patch to
/// future turns only.
fn apply_config(vm: &mut SessionViewModel, patch: &ConfigPatch) {
    if let Some(model) = &patch.model {
        vm.model = model.clone();
    }
    if let Some(provider) = &patch.provider {
        vm.provider = provider.clone();
    }
    if patch.approval_mode.is_some() {
        vm.approval_mode = patch.approval_mode;
    }
    if let Some(policy) = patch.sandbox {
        vm.sandbox = policy;
    }
    if let Some(allow) = &patch.allow {
        vm.allow_tools = allow.clone();
    }
    if let Some(deny) = &patch.deny {
        vm.deny_tools = deny.clone();
    }
    if let Some(goal) = &patch.goal {
        vm.goal = goal.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_url_uses_the_protocol_port() {
        assert_eq!(default_url(), "ws://127.0.0.1:7699");
    }

    #[test]
    fn client_name_is_set() {
        assert_eq!(CLIENT_NAME, "modron-mobile");
    }

    #[test]
    fn url_validation() {
        assert!(validate_ws_url("ws://127.0.0.1:7699").is_ok());
        assert!(validate_ws_url("wss://modron.example.com").is_ok());
        assert!(validate_ws_url("http://127.0.0.1:7699").is_err());
        assert!(validate_ws_url("ws://").is_err());
        assert!(validate_ws_url("ws:///path").is_err());
    }

    /// Fold one frame into a fresh shared state and return the snapshot.
    fn folded(frame: Frame) -> ViewModelSnapshot {
        let shared = Shared::new();
        fold_frame(&shared, frame);
        let st = shared.state.lock().unwrap();
        ViewModelSnapshot::from_vm(&st.vm)
    }

    #[test]
    fn remote_control_started_lands_in_the_note() {
        // Desktop parity: the share URL surfaces through last_note (the
        // TSX renders the note in the chat scrollback).
        let snap = folded(Frame::event(
            None,
            0,
            Payload::RemoteControlStarted {
                url: "https://bridge.example.com/s/abc".into(),
            },
        ));
        assert_eq!(
            snap.last_note.as_deref(),
            Some("remote-control live: https://bridge.example.com/s/abc - open on any device")
        );
    }

    #[test]
    fn reasoning_deltas_gate_on_show_reasoning() {
        let session = Uuid::new_v4();
        let delta = |text: &str, seq: u64| {
            Frame::event(
                Some(session),
                seq,
                Payload::Delta {
                    delta: WireDelta::Reasoning {
                        text: text.to_string(),
                    },
                },
            )
        };
        // Default is off: reasoning deltas never reach the folded state.
        let shared = Shared::new();
        fold_frame(&shared, delta("step one ", 1));
        fold_frame(&shared, delta("step two", 2));
        {
            let st = shared.state.lock().unwrap();
            assert!(!st.vm.show_reasoning, "show_reasoning defaults off");
            assert_eq!(st.vm.streaming_reasoning, "", "dropped while off");
            assert_eq!(st.vm.last_seq, 2, "the resume cursor still advances");
        }
        // Opt in (the bridge's set_show_reasoning): deltas land.
        let client = RemoteClient {
            shared: Arc::new(shared),
        };
        client.set_show_reasoning(true);
        assert!(client.state().show_reasoning);
        fold_frame(
            &client.shared,
            Frame::event(
                Some(session),
                3,
                Payload::Delta {
                    delta: WireDelta::Reasoning {
                        text: "visible now".into(),
                    },
                },
            ),
        );
        assert_eq!(client.state().streaming_reasoning, "visible now");
        // Off again: the buffer clears (TUI toggle parity) and drops.
        client.set_show_reasoning(false);
        assert_eq!(client.state().streaming_reasoning, "");
        fold_frame(&client.shared, delta("gone", 4));
        assert_eq!(client.state().streaming_reasoning, "");
    }
}
