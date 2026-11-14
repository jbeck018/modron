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
use tokio::sync::{mpsc, oneshot, watch};
use uuid::Uuid;

/// The `client` string sent in the hello frame.
pub const CLIENT_NAME: &str = "modron-mobile";

/// Loopback URL for the default remote port (TLS terminates in front of
/// the server; the client speaks wss:// in production).
pub fn default_url() -> String {
    format!("ws://127.0.0.1:{DEFAULT_PORT}")
}

/// Deadline for the whole connect: TCP/TLS open plus the hello/welcome
/// handshake (native), or the browser open plus welcome (web).
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// Protocol keepalive: a `ping` every `ping_every` while connected. A
/// connection with no inbound frame for `idle_timeout` counts as dropped
/// (checked on the ping cadence, so the drop lands within one `ping_every`
/// after that window).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeepaliveConfig {
    pub ping_every: Duration,
    pub idle_timeout: Duration,
}

impl Default for KeepaliveConfig {
    fn default() -> Self {
        Self {
            ping_every: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(90),
        }
    }
}

/// Per-client connect settings. Defaults: loopback-only plain `ws://`,
/// the 15 second connect deadline, and the 30s/90s keepalive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConnectOptions {
    /// Allow plain `ws://` to a non-loopback host (trusted LAN only). The
    /// shell's explicit opt-in toggle; `wss://` never needs it.
    pub allow_insecure: bool,
    /// Deadline for connect plus handshake.
    pub connect_timeout: Duration,
    pub keepalive: KeepaliveConfig,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            allow_insecure: false,
            connect_timeout: HANDSHAKE_TIMEOUT,
            keepalive: KeepaliveConfig::default(),
        }
    }
}

/// The one notice a connection shows when it receives a frame this client
/// cannot decode (shared copy: the same words in every client).
pub const UNDECODABLE_NOTE: &str =
    "received a frame this client version cannot read - some updates may be missing; upgrade modron";

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
///
/// Plain `ws://` is only accepted for loopback hosts (`localhost`,
/// `127.0.0.0/8`, `::1`) unless `allow_insecure` is set: a non-loopback
/// device must be reached over `wss://`, or the device token travels in clear.
fn validate_ws_url(url: &str, allow_insecure: bool) -> Result<(), ClientError> {
    let (secure, after_scheme) = match url.strip_prefix("wss://") {
        Some(rest) => (true, rest),
        None => (
            false,
            url.strip_prefix("ws://").ok_or_else(|| {
                ClientError::Connect("url must start with ws:// or wss://".into())
            })?,
        ),
    };
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    // Userinfo ends at the last '@'; anything before it is not the host.
    let host_port = authority.rsplit('@').next().unwrap_or("");
    let host = url_host(host_port);
    if host.is_empty() {
        return Err(ClientError::Connect("url has no host".into()));
    }
    if !secure && !allow_insecure && !is_loopback_host(host) {
        return Err(ClientError::Connect(
            "ws:// is only allowed to loopback hosts; use wss:// for this device".into(),
        ));
    }
    Ok(())
}

/// The host part of a `host[:port]` or `[v6][:port]` authority.
fn url_host(host_port: &str) -> &str {
    if let Some(bracketed) = host_port.strip_prefix('[') {
        return bracketed.split(']').next().unwrap_or("");
    }
    host_port.split(':').next().unwrap_or("")
}

/// True for `localhost` and any IPv4 `127.0.0.0/8` or IPv6 `::1` address.
fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
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
/// The uncorrelated hello frame (id 0 is reserved for it) - shared
/// construction via modron_protocol::client.
pub(crate) fn hello_frame(token: Option<&str>) -> Frame {
    modron_protocol::client::hello_frame(CLIENT_NAME, token.map(Into::into))
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
    /// The current connection's command side; `None` when down. Lock
    /// order: `live` before `state` (every path that takes both follows
    /// it), so the generation checks and the installs are serialized.
    live: Mutex<Option<Live>>,
    /// Flips to true once the connection dies (`wait_closed`).
    closed_tx: watch::Sender<bool>,
    closed_rx: watch::Receiver<bool>,
    /// Connection generation: a stale loop's shutdown must not clobber a
    /// newer connection's state (reconnect while an old loop is still
    /// draining).
    gen: AtomicU64,
    /// Request-id counter (0 is reserved for the uncorrelated hello).
    next_id: AtomicU64,
    /// Connect deadline and keepalive, fixed for this client's lifetime.
    options: ConnectOptions,
}

#[derive(Default)]
struct CoreState {
    vm: SessionViewModel,
    fold: FoldState,
}

/// One installed connection: the outgoing frame channel the UI-facing
/// methods write to, and the signal that makes its run loop send a close
/// frame and exit. Holding `tx` here (not in `establish`) is what lets the
/// run loop see the channel close.
struct Live {
    tx: mpsc::UnboundedSender<Frame>,
    close: oneshot::Sender<()>,
}

/// Retire the installed connection, if any: ask its run loop to close the
/// socket and drop the outgoing channel. Caller holds the `live` lock.
fn retire(slot: &mut Option<Live>) {
    if let Some(old) = slot.take() {
        let _ = old.close.send(());
    }
}

impl Shared {
    fn new(options: ConnectOptions) -> Self {
        let (closed_tx, closed_rx) = watch::channel(false);
        Self {
            state: Mutex::new(CoreState::default()),
            live: Mutex::new(None),
            closed_tx,
            closed_rx,
            gen: AtomicU64::new(0),
            next_id: AtomicU64::new(0),
            options,
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
        Self::connect_with_options(url, token, ConnectOptions::default())
    }

    /// [`Self::connect`] with explicit options: the LAN `ws://` opt-in
    /// (`allow_insecure`), the connect deadline, and the keepalive timing.
    pub fn connect_with_options(
        url: &str,
        token: Option<&str>,
        options: ConnectOptions,
    ) -> Result<Self, ClientError> {
        validate_ws_url(url, options.allow_insecure)?;
        let client = Self {
            shared: Arc::new(Shared::new(options)),
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
        self.reconnect_with_options(url, session_id, token, false)
    }

    /// [`Self::reconnect`] with the LAN `ws://` opt-in (`allow_insecure`),
    /// passed through to the URL check exactly as at connect time.
    pub fn reconnect_with_options(
        &self,
        url: &str,
        session_id: Uuid,
        token: Option<&str>,
        allow_insecure: bool,
    ) -> Result<(), ClientError> {
        validate_ws_url(url, allow_insecure)?;
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
        // Tear down any previous live connection first, and supersede any
        // handshake still in flight: its late welcome is then ignored.
        let gen = {
            let mut slot = self.shared.live.lock().unwrap();
            let gen = self.shared.gen.fetch_add(1, Ordering::SeqCst) + 1;
            retire(&mut slot);
            gen
        };
        {
            let mut st = self.shared.state.lock().unwrap();
            st.vm.connection = crate::view::ConnectionState::Connecting;
            st.vm.last_error = None;
            st.vm.sessions.clear();
            st.vm.pending_approvals.clear();
        }
        let _ = self.shared.closed_tx.send(false);
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

    /// Take the front pending approval out of the queue (the wire truth
    /// stays authoritative: the server's `ApprovalResolved` echo removes
    /// the rest). The next queued request becomes the front. Non-destructive
    /// reads use `state().pending_approval`.
    pub fn take_pending_approval(&self) -> Option<PendingApprovalView> {
        let mut st = self.shared.state.lock().unwrap();
        st.vm.pending_approvals.pop_front()
    }

    /// Frames this client could not decode, since the client was created
    /// (each one also counts toward the single per-connection notice).
    pub fn undecodable_frames(&self) -> u64 {
        self.shared.state.lock().unwrap().vm.undecodable_frames
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
        let request_id = request_id.into();
        self.send_frame(Frame::request(
            self.next_request_id(),
            Payload::Approve {
                request_id: request_id.clone(),
                decision,
            },
        ))?;
        // Answered: the view moves on to the next queued request now. The
        // server's ApprovalResolved echo is then a no-op for this id.
        let mut st = self.shared.state.lock().unwrap();
        crate::view::resolve_approval(&mut st.vm, &request_id);
        Ok(())
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

    /// Close the connection: the live run loop sends a close frame and
    /// exits (the server sees the socket close), any handshake still in
    /// flight is superseded, and the state goes `Disconnected`. Resolves
    /// [`Self::wait_closed`] immediately; the close frame itself goes out
    /// on the run loop's next poll.
    pub fn disconnect(&self) {
        let mut slot = self.shared.live.lock().unwrap();
        self.shared.gen.fetch_add(1, Ordering::SeqCst);
        retire(&mut slot);
        {
            let mut st = self.shared.state.lock().unwrap();
            st.vm.connection = crate::view::ConnectionState::Disconnected;
            st.vm.last_error = None;
            st.vm.pending_approvals.clear();
        }
        drop(slot);
        let _ = self.shared.closed_tx.send(true);
    }

    // -- internals --------------------------------------------------------

    fn next_request_id(&self) -> u64 {
        self.shared.next_request_id()
    }

    fn send_frame(&self, frame: Frame) -> Result<(), ClientError> {
        let guard = self.shared.live.lock().unwrap();
        match &*guard {
            Some(live) => live.tx.send(frame).map_err(|_| ClientError::Closed),
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
    // One deadline over connect plus handshake, on every transport: a
    // server that accepts the socket but never welcomes fails here with a
    // clear error instead of hanging the connecting state.
    let deadline = shared.options.connect_timeout;
    let attempt = transport::handshake(&url, token.as_deref());
    let handshake = match with_timeout(deadline, attempt).await {
        Some(result) => result,
        None => Err(ClientError::Connect(format!(
            "timed out after {deadline:?} waiting to connect and receive the server's welcome"
        ))),
    };
    match handshake {
        Ok((mut conn, server_version)) => {
            let (tx, rx) = mpsc::unbounded_channel();
            let (close, close_rx) = oneshot::channel();
            // The generation check and the install happen under one lock,
            // so a handshake superseded by connect/disconnect cannot replace
            // the newer connection's state.
            let installed = {
                let mut slot = shared.live.lock().unwrap();
                if shared.gen.load(Ordering::SeqCst) != gen {
                    false
                } else {
                    if let Some((session_id, last_seq)) = resume {
                        // Queued before any caller frame can interleave.
                        let _ = tx.send(Frame::request(
                            shared.next_request_id(),
                            Payload::Attach {
                                session_id,
                                last_seq,
                            },
                        ));
                    }
                    *slot = Some(Live { tx, close });
                    let mut st = shared.state.lock().unwrap();
                    st.vm.connection = crate::view::ConnectionState::Connected;
                    st.vm.server_version = server_version;
                    true
                }
            };
            if !installed {
                conn.close().await;
                return;
            }
            run_loop(&mut conn, shared, gen, rx, close_rx).await;
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
    mut close_rx: oneshot::Receiver<()>,
) {
    let keepalive = shared.options.keepalive;
    // Keepalive runs on one timer (one per ping period, never per frame).
    // Each tick sends a ping and checks whether any inbound frame arrived
    // since the previous tick; `silent_limit` consecutive silent ticks mean
    // the idle window has passed, so the drop lands between `idle_timeout`
    // and `idle_timeout + ping_every` after the last frame.
    let ping_ms = keepalive.ping_every.as_millis().max(1);
    let silent_limit = keepalive.idle_timeout.as_millis().div_ceil(ping_ms).max(1);
    let mut next_ping = Box::pin(delay(keepalive.ping_every));
    let mut heard_since_tick = false;
    let mut silent_ticks: u128 = 0;
    // One undecodable-frame notice per connection (this loop is one
    // connection; a reconnect runs a fresh loop with the flag cleared).
    let mut noted_undecodable = false;
    let reason: Option<String> = loop {
        tokio::select! {
            // Retired by disconnect() or a newer connect(): close the socket.
            _ = &mut close_rx => {
                conn.close().await;
                break None;
            }
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
            msg = conn.next_in() => {
                if matches!(msg, Some(Ok(_))) {
                    heard_since_tick = true;
                }
                match msg {
                    Some(Ok(In::Text(json))) => match serde_json::from_str::<Frame>(&json) {
                        Ok(frame) => fold_frame(&shared, frame),
                        Err(e) => {
                            tracing::warn!(error = %e, "inbound frame decode failed");
                            note_undecodable(&shared, &mut noted_undecodable);
                        }
                    },
                    Some(Ok(In::Open)) => {}
                    Some(Ok(In::Closed)) | None => {
                        break Some("server closed the connection".into());
                    }
                    Some(Err(e)) => break Some(e.to_string()),
                }
            },
            _ = &mut next_ping => {
                next_ping = Box::pin(delay(keepalive.ping_every));
                if std::mem::take(&mut heard_since_tick) {
                    silent_ticks = 0;
                } else {
                    silent_ticks += 1;
                }
                if silent_ticks >= silent_limit {
                    conn.close().await;
                    break Some(format!(
                        "connection lost: no frames from the server for {:?}",
                        keepalive.idle_timeout
                    ));
                }
                let ping = Frame::request(shared.next_request_id(), Payload::Ping {});
                let sent = match serde_json::to_string(&ping) {
                    Ok(json) => conn.send_text(&json).await,
                    Err(e) => Err(ClientError::Json(e.to_string())),
                };
                if let Err(e) = sent {
                    break Some(format!("keepalive failed: {e}"));
                }
            },
        }
    };
    shutdown(&shared, gen, reason);
}

/// Count one undecodable frame; the first in a connection also raises the
/// shared notice (later ones only count).
fn note_undecodable(shared: &Shared, noted: &mut bool) {
    let mut st = shared.state.lock().unwrap();
    st.vm.undecodable_frames += 1;
    if !*noted {
        *noted = true;
        st.vm.last_note = Some(UNDECODABLE_NOTE.into());
    }
}

/// Mark the connection down and release the command channel. Guarded by
/// generation so a stale loop (reconnect in progress) leaves the newer
/// connection's state alone. `None` = clean close (no error surface);
/// `Some` = the cause string.
fn shutdown(shared: &Shared, gen: u64, reason: Option<String>) {
    let mut slot = shared.live.lock().unwrap();
    if shared.gen.load(Ordering::SeqCst) != gen {
        return;
    }
    *slot = None;
    {
        let mut st = shared.state.lock().unwrap();
        st.vm.connection = crate::view::ConnectionState::Disconnected;
        st.vm.last_error = reason;
        st.vm.pending_approvals.clear();
    }
    drop(slot);
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
            crate::view::enqueue_approval(vm, request_id, &call);
        }
        Payload::ApprovalResolved { request_id, .. } => {
            crate::view::resolve_approval(vm, &request_id);
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
        assert!(validate_ws_url("ws://127.0.0.1:7699", false).is_ok());
        assert!(validate_ws_url("wss://modron.example.com", false).is_ok());
        assert!(validate_ws_url("http://127.0.0.1:7699", false).is_err());
        assert!(validate_ws_url("ws://", false).is_err());
        assert!(validate_ws_url("ws:///path", false).is_err());
    }

    /// Fold one frame into a fresh shared state and return the snapshot.
    fn folded(frame: Frame) -> ViewModelSnapshot {
        let shared = Shared::new(ConnectOptions::default());
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
        let shared = Shared::new(ConnectOptions::default());
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

    #[test]
    fn plain_ws_is_only_accepted_for_loopback_hosts() {
        assert!(validate_ws_url("ws://127.0.0.1:7699", false).is_ok());
        assert!(validate_ws_url("ws://127.8.9.10:7699/", false).is_ok());
        assert!(validate_ws_url("ws://localhost:7699", false).is_ok());
        assert!(validate_ws_url("ws://LOCALHOST:7699", false).is_ok());
        assert!(validate_ws_url("ws://[::1]:7699", false).is_ok());
        assert!(validate_ws_url("ws://example.com", false).is_err());
        assert!(validate_ws_url("ws://example.com:7699/path", false).is_err());
        assert!(validate_ws_url("ws://192.168.1.20:7699", false).is_err());
        assert!(validate_ws_url("ws://[2001:db8::1]:7699", false).is_err());
        // Userinfo cannot make a remote host look loopback.
        assert!(validate_ws_url("ws://127.0.0.1@example.com", false).is_err());
        assert!(validate_ws_url("ws://localhost.example.com", false).is_err());
    }

    #[test]
    fn wss_is_accepted_for_any_host() {
        assert!(validate_ws_url("wss://example.com", false).is_ok());
        assert!(validate_ws_url("wss://192.168.1.20:7699", false).is_ok());
    }

    #[test]
    fn allow_insecure_lets_plain_ws_reach_a_remote_host() {
        assert!(validate_ws_url("ws://example.com", true).is_ok());
        // The flag only relaxes ws://; a malformed URL is still refused.
        assert!(validate_ws_url("ws://", true).is_err());
        assert!(validate_ws_url("http://example.com", true).is_err());
    }
}
