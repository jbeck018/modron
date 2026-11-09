//! `RemoteClient` — the mobile remote-control client: a WebSocket to the
//! modron server, one background task folding every inbound [`Frame`] into
//! a [`SessionViewModel`] behind a std mutex, and a polling API for
//! UI shells (`state()` / `take_new_entries()` / `take_pending_approval()`
//! plus one method per client command). No callbacks: a native shell ticks
//! the snapshot on its render loop.
//!
//! ```ignore
//! let client = RemoteClient::connect(&url, None).await?;
//! client.attach(session_id, 0).await?;          // awaits the Snapshot fold
//! // render loop:
//! let snap = client.state();                     // cheap clone, serde-ready
//! for entry in client.take_new_entries() { ... } // incremental rows
//! client.submit(Some(session_id), "fix the bug")?;
//! ```
//!
//! Reconnect is explicit (`reconnect`), resuming from the last seen
//! per-session `seq` so the server replays exactly the missed events.

use crate::error::ClientError;
use crate::view::{
    fold_snapshot, fold_turn_done, fold_turn_unfinished, FoldState, OAuthProgressView,
    PendingApprovalView, SessionViewModel, ViewModelSnapshot,
};
use futures_util::{SinkExt, StreamExt};
use modron_core::ToolActivity;
use modron_protocol::{
    ConfigPatch, Decision, Frame, Payload, SessionMetaWire, WireDelta, DEFAULT_PORT,
    PROTOCOL_VERSION,
};
use modron_view::TranscriptEntry;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use uuid::Uuid;

/// The `client` string sent in the hello frame.
pub const CLIENT_NAME: &str = "modron-mobile";

/// Loopback URL for the default remote port (TLS terminates in front of
/// the server; the client speaks wss:// in production).
pub fn default_url() -> String {
    format!("ws://127.0.0.1:{DEFAULT_PORT}")
}

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(15);

type WsStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
type WsSink = futures_util::stream::SplitSink<WsStream, WsMessage>;
type WsRead = futures_util::stream::SplitStream<WsStream>;

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

/// Everything the recv loop and the UI-facing methods share.
struct Shared {
    /// View model + fold bookkeeping + correlated-request waiters.
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
    /// In-flight correlated requests: id -> waiter.
    requests: HashMap<u64, oneshot::Sender<Result<Payload, ClientError>>>,
    /// Tasks waiting for the next Snapshot fold (`attach`).
    snapshots: Vec<oneshot::Sender<Result<(), ClientError>>>,
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
    /// Connect and run the hello/welcome handshake. Fails on socket
    /// errors, timeouts, and protocol-version mismatches.
    pub async fn connect(url: &str, token: Option<&str>) -> Result<Self, ClientError> {
        let client = Self {
            shared: Arc::new(Shared::new()),
        };
        client.spin_up(url, token).await?;
        Ok(client)
    }

    /// Reconnect and re-`attach` from the last seen seq: the server
    /// replays every event the client missed while offline.
    pub async fn reconnect(
        &self,
        url: &str,
        session_id: Uuid,
        token: Option<&str>,
    ) -> Result<(), ClientError> {
        let last_seq = {
            let st = self.shared.state.lock().unwrap();
            st.vm.last_seq
        };
        self.spin_up(url, token).await?;
        self.attach(session_id, last_seq).await
    }

    /// Mark connecting, run the handshake, spawn the run loop. Bumps the
    /// generation so a still-draining old loop cannot clobber state.
    async fn spin_up(&self, url: &str, token: Option<&str>) -> Result<(), ClientError> {
        {
            let mut st = self.shared.state.lock().unwrap();
            st.vm.connection = crate::view::ConnectionState::Connecting;
            st.vm.last_error = None;
        }
        let gen = self.shared.gen.fetch_add(1, Ordering::SeqCst) + 1;
        match handshake(url, token).await {
            Ok((sink, stream, server_version)) => {
                {
                    let mut st = self.shared.state.lock().unwrap();
                    st.vm.connection = crate::view::ConnectionState::Connected;
                    st.vm.server_version = server_version;
                }
                let (tx, rx) = mpsc::unbounded_channel();
                *self.shared.cmd.lock().unwrap() = Some(tx);
                spawn_loop(self.shared.clone(), gen, sink, stream, rx);
                Ok(())
            }
            Err(e) => {
                let mut st = self.shared.state.lock().unwrap();
                st.vm.connection = crate::view::ConnectionState::Disconnected;
                st.vm.last_error = Some(e.to_string());
                Err(e)
            }
        }
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

    /// Resolves when the connection dies (server close, network loss).
    /// Errors (sender dropped with the last client clone) count as closed.
    pub async fn wait_closed(&self) {
        let mut rx = self.shared.closed_rx.clone();
        let _ = rx.wait_for(|closed| *closed).await;
    }

    // -- commands ---------------------------------------------------------

    /// Subscribe to a session's events; resolves once the `Snapshot` has
    /// folded. `last_seq = 0` asks for the full replay.
    pub async fn attach(&self, session_id: Uuid, last_seq: u64) -> Result<(), ClientError> {
        let (tx, rx) = oneshot::channel();
        {
            let mut st = self.shared.state.lock().unwrap();
            st.snapshots.push(tx);
        }
        self.send_frame(Frame::request(
            self.next_request_id(),
            Payload::Attach {
                session_id,
                last_seq,
            },
        ))?;
        let reply = timeout(SNAPSHOT_TIMEOUT, rx)
            .await
            .map_err(|_| ClientError::Timeout);
        match reply {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(_)) => Err(ClientError::Closed),
            Err(e) => Err(e),
        }
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
    /// (`allow once` / `allow for this run` / `deny`); allow-all is
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

    /// Model switch, TUI /model parity: pins the model AND follows the
    /// provider in one full patch.
    pub fn select_model(
        &self,
        model: impl Into<String>,
        provider: impl Into<String>,
    ) -> Result<(), ClientError> {
        self.set_config(ConfigPatch {
            model: Some(model.into()),
            provider: Some(provider.into()),
            ..ConfigPatch::default()
        })
    }

    /// List sessions (correlated request; the answer also lands in
    /// `state().sessions`).
    pub async fn list_sessions(&self) -> Result<Vec<SessionMetaWire>, ClientError> {
        match self.request(Payload::ListSessions {}).await? {
            Payload::Sessions { sessions } => Ok(sessions),
            other => Err(ClientError::Request(format!(
                "unexpected list_sessions reply: {other:?}"
            ))),
        }
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

    /// Close the connection (the run loop drains, sends a close frame,
    /// and marks the state disconnected).
    pub fn disconnect(&self) {
        *self.shared.cmd.lock().unwrap() = None;
    }

    // -- internals --------------------------------------------------------

    fn next_request_id(&self) -> u64 {
        self.shared.next_id.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn send_frame(&self, frame: Frame) -> Result<(), ClientError> {
        let guard = self.shared.cmd.lock().unwrap();
        match &*guard {
            Some(tx) => tx.send(frame).map_err(|_| ClientError::Closed),
            None => Err(ClientError::Closed),
        }
    }

    /// Fire one correlated request and await its answer.
    async fn request(&self, payload: Payload) -> Result<Payload, ClientError> {
        let id = self.next_request_id();
        let (tx, rx) = oneshot::channel();
        {
            let mut st = self.shared.state.lock().unwrap();
            st.requests.insert(id, tx);
        }
        self.send_frame(Frame::request(id, payload))?;
        match timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(reply)) => reply,
            Ok(Err(_)) => Err(ClientError::Closed),
            Err(_) => Err(ClientError::Timeout),
        }
    }
}

// ---------------------------------------------------------------------------
// Handshake + run loop
// ---------------------------------------------------------------------------

async fn handshake(
    url: &str,
    token: Option<&str>,
) -> Result<(WsSink, WsRead, String), ClientError> {
    let (ws, _) = connect_async(url)
        .await
        .map_err(|e| ClientError::Connect(e.to_string()))?;
    let (mut sink, mut stream) = ws.split();
    let hello = Frame::request(
        0,
        Payload::Hello {
            client: CLIENT_NAME.into(),
            protocol: PROTOCOL_VERSION,
            token: token.map(Into::into),
        },
    );
    let json = serde_json::to_string(&hello).map_err(|e| ClientError::Json(e.to_string()))?;
    sink.send(WsMessage::Text(json.into()))
        .await
        .map_err(|e| ClientError::Handshake(e.to_string()))?;
    let msg = timeout(HANDSHAKE_TIMEOUT, stream.next())
        .await
        .map_err(|_| ClientError::Timeout)?;
    let frame = match msg {
        Some(Ok(WsMessage::Text(json))) => {
            serde_json::from_str::<Frame>(&json).map_err(|e| ClientError::Json(e.to_string()))?
        }
        Some(Ok(_)) => return Err(ClientError::Handshake("expected a welcome frame".into())),
        Some(Err(e)) => return Err(ClientError::Handshake(e.to_string())),
        None => return Err(ClientError::Handshake("closed during handshake".into())),
    };
    match frame.payload {
        Payload::Welcome {
            server_version,
            protocol,
        } if protocol == PROTOCOL_VERSION => Ok((sink, stream, server_version)),
        Payload::Welcome { protocol, .. } => Err(ClientError::ProtocolMismatch {
            server: protocol,
            client: PROTOCOL_VERSION,
        }),
        other => Err(ClientError::Handshake(format!(
            "expected welcome, got {other:?}"
        ))),
    }
}

fn spawn_loop(
    shared: Arc<Shared>,
    gen: u64,
    sink: WsSink,
    stream: WsRead,
    mut rx: mpsc::UnboundedReceiver<Frame>,
) {
    tokio::spawn(async move {
        let mut sink = sink;
        let mut stream = stream;
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
                        if let Err(e) = sink.send(WsMessage::Text(json.into())).await {
                            break Some(format!("send failed: {e}"));
                        }
                    }
                    None => {
                        let _ = sink.send(WsMessage::Close(None)).await;
                        break None;
                    }
                },
                msg = stream.next() => match msg {
                    Some(Ok(WsMessage::Text(json))) => match serde_json::from_str::<Frame>(&json) {
                        Ok(frame) => fold_frame(&shared, frame),
                        Err(e) => tracing::warn!(error = %e, "inbound frame decode failed"),
                    },
                    Some(Ok(WsMessage::Ping(data))) => {
                        let _ = sink.send(WsMessage::Pong(data)).await;
                    }
                    Some(Ok(WsMessage::Close(_))) | None => {
                        break Some("server closed the connection".into());
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => break Some(e.to_string()),
                },
            }
        };
        shutdown(&shared, gen, reason);
    });
}

/// Mark the connection down, fail in-flight waiters, and release the
/// command channel. Guarded by generation so a stale loop (reconnect in
/// progress) leaves the newer connection's state alone. `None` = clean
/// close (no error surface); `Some` = the cause string.
fn shutdown(shared: &Shared, gen: u64, reason: Option<String>) {
    if shared.gen.load(Ordering::SeqCst) != gen {
        return;
    }
    {
        let mut st = shared.state.lock().unwrap();
        st.vm.connection = crate::view::ConnectionState::Disconnected;
        st.vm.last_error = reason.clone();
        let closed = ClientError::Closed;
        for (_, waiter) in st.requests.drain() {
            let _ = waiter.send(Err(closed.clone()));
        }
        for waiter in st.snapshots.drain(..) {
            let _ = waiter.send(Err(ClientError::Closed));
        }
    }
    *shared.cmd.lock().unwrap() = None;
    let _ = shared.closed_tx.send(true);
}

// ---------------------------------------------------------------------------
// Frame folding (one defined effect per payload, per modron-protocol docs)
// ---------------------------------------------------------------------------

fn fold_frame(shared: &Shared, frame: Frame) {
    let mut st = shared.state.lock().unwrap();
    let CoreState {
        vm,
        fold,
        requests,
        snapshots,
    } = &mut *st;

    // Correlated replies resolve their waiter first (and still fold).
    if let Some(id) = frame.id {
        if let Some(waiter) = requests.remove(&id) {
            let _ = waiter.send(Ok(frame.payload.clone()));
        }
    }
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
            for waiter in snapshots.drain(..) {
                let _ = waiter.send(Ok(()));
            }
        }
        Payload::Event { event } => fold.fold_event(vm, event),
        Payload::Delta { delta } => match delta {
            WireDelta::Text { text } => vm.streaming_text.push_str(&text),
            WireDelta::Reasoning { text } => vm.streaming_reasoning.push_str(&text),
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
        Payload::Error { message } => {
            vm.last_error = Some(message.clone());
            for waiter in snapshots.drain(..) {
                let _ = waiter.send(Err(ClientError::Request(message.clone())));
            }
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
}
