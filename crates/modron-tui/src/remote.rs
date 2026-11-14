//! remote.rs — the TUI as a remote-control client (transport plumbing).
//!
//! Connects to a `modron serve` process over a Unix socket (NDJSON lines,
//! the local fast path) or a WebSocket (remote; TLS terminates in front of
//! the server via tailscale/cloudflared). Only frame<->wire lives here:
//! translating frames into the run loop's channels is `run`'s job, where
//! the internal event types are defined.
//!
//! Handshake: Hello { protocol } -> Welcome { protocol }; a mismatching
//! peer is refused before any session traffic.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use futures::{SinkExt, StreamExt};
use modron_core::SessionEvent;
use modron_protocol::{Frame, LiveState, Payload};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use uuid::Uuid;

/// Handshake deadline: a server that cannot answer Welcome in 5s is not a
/// modron server (or is wedged) — fail fast into the in-process fallback.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Reply deadline for a correlated request (attach).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Query parameter that carries a paired device token on a ws(s) target
/// (`ws://host:7699/?token=...`). Stripped before the URL is dialed or shown.
const TOKEN_PARAM: &str = "token";

/// Environment fallback for the device token when the URL carries none.
const TOKEN_ENV: &str = "MODRON_DEVICE_TOKEN";

/// Split a `token` query parameter off a ws(s) target. Returns the target
/// without that parameter (safe to log or show) and the token, if any.
/// Non-ws targets (unix sockets) pass through untouched.
fn split_token_param(target: &str) -> (String, Option<String>) {
    let is_ws = target.starts_with("ws://") || target.starts_with("wss://");
    let Some((base, query)) = target.split_once('?').filter(|_| is_ws) else {
        return (target.to_string(), None);
    };
    let mut token = None;
    let kept: Vec<&str> = query
        .split('&')
        .filter(|pair| match pair.split_once('=') {
            Some((TOKEN_PARAM, value)) => {
                if !value.is_empty() {
                    token = Some(value.to_string());
                }
                false
            }
            _ => true,
        })
        .collect();
    let stripped = if kept.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{}", kept.join("&"))
    };
    (stripped, token)
}

/// Resolve the device token and the target to dial: a `token` URL parameter
/// wins, then the `MODRON_DEVICE_TOKEN` env var (read through `env` so tests
/// never touch the process environment), else no token.
fn resolve_device_token(
    target: &str,
    env: impl Fn(&str) -> Option<String>,
) -> (String, Option<String>) {
    let (stripped, from_url) = split_token_param(target);
    let token = from_url.or_else(|| env(TOKEN_ENV).filter(|t| !t.is_empty()));
    (stripped, token)
}

/// The `hello` frame for this client, carrying the device token when set.
fn client_hello(token: Option<String>) -> Frame {
    modron_protocol::client::hello_frame("modron-tui", token)
}

/// The default Unix socket path (`~/.modron/serve.sock`).
pub fn default_socket_path() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".modron").join("serve.sock")
}

/// The reply to a successful attach: the replayable event log plus the
/// live (non-persisted) view at attach time.
#[derive(Clone, Debug)]
pub struct AttachSnapshot {
    pub session_id: Uuid,
    pub last_seq: u64,
    pub events: Vec<SessionEvent>,
    pub live: LiveState,
}

// ---------------------------------------------------------------------------
// Link: one duplex frame channel per transport
// ---------------------------------------------------------------------------

/// Shown once per connection when the server sends a frame this client
/// cannot decode (a newer server; some updates may be missing).
pub const UNDECODABLE_NOTICE: &str = modron_view::UNDECODABLE_FRAME_NOTICE;

/// One inbound item from the server: a decoded frame, or a frame this
/// client could not decode.
#[derive(Debug)]
pub enum Inbound {
    Frame(Box<Frame>),
    Undecodable,
}

/// Undecodable-frame bookkeeping for one connection. The user-visible
/// notice is raised at most once per connection, however many arrive.
#[derive(Debug, Default)]
pub struct UndecodableFrames {
    count: u64,
    announced: bool,
}

impl UndecodableFrames {
    pub fn record(&mut self) {
        self.count += 1;
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    /// True exactly once: the first time any frame has been undecodable.
    pub fn take_notice(&mut self) -> bool {
        if self.count > 0 && !self.announced {
            self.announced = true;
            true
        } else {
            false
        }
    }
}

enum Link {
    Unix {
        write: OwnedWriteHalf,
        lines: tokio::io::Lines<BufReader<OwnedReadHalf>>,
    },
    // Boxed: WebSocketStream is ~5x the Unix variant's size and the enum
    // is a hot handle in the pump's select loop.
    Ws {
        stream: Box<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>>,
    },
}

impl Link {
    async fn open(target: &str) -> Result<Self> {
        let path = match target.strip_prefix("unix:") {
            Some(explicit) => PathBuf::from(explicit),
            None if target == "unix" => default_socket_path(),
            None => {
                return if target.starts_with("ws://") || target.starts_with("wss://") {
                    // non_exhaustive: default-then-mutate.
                    let mut config =
                        tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default();
                    config.max_message_size = Some(256 * 1024 * 1024);
                    config.max_frame_size = Some(16 * 1024 * 1024);
                    let (stream, _) =
                        tokio_tungstenite::connect_async_with_config(target, Some(config), false)
                            .await
                            .with_context(|| format!("connecting to {target}"))?;
                    Ok(Self::Ws {
                        stream: Box::new(stream),
                    })
                } else {
                    bail!("unsupported connect target {target:?} (use \"unix\", \"unix:<path>\" or \"ws://host:port\")");
                };
            }
        };
        let stream = UnixStream::connect(&path).await.with_context(|| {
            format!(
                "no server at {} (start one with: modron serve)",
                path.display()
            )
        })?;
        let (read, write) = stream.into_split();
        Ok(Self::Unix {
            write,
            lines: BufReader::new(read).lines(),
        })
    }

    async fn send(&mut self, frame: &Frame) -> Result<()> {
        let line = serde_json::to_string(frame).context("frame serialization")?;
        match self {
            Self::Unix { write, .. } => {
                write.write_all(line.as_bytes()).await?;
                write.write_all(b"\n").await?;
            }
            Self::Ws { stream } => {
                stream.send(WsMessage::Text(line.into())).await?;
            }
        }
        Ok(())
    }

    /// Next inbound item, or None when the peer closed.
    async fn recv(&mut self) -> Option<Inbound> {
        match self {
            Self::Unix { lines, .. } => loop {
                match lines.next_line().await {
                    Ok(Some(line)) if line.trim().is_empty() => continue,
                    Ok(Some(line)) => match serde_json::from_str(&line) {
                        Ok(frame) => return Some(Inbound::Frame(Box::new(frame))),
                        Err(e) => {
                            tracing::warn!(error = %e, "undecodable line from server");
                            return Some(Inbound::Undecodable);
                        }
                    },
                    _ => return None,
                }
            },
            Self::Ws { stream } => loop {
                match stream.next().await {
                    Some(Ok(WsMessage::Text(text))) => match serde_json::from_str(&text) {
                        Ok(frame) => return Some(Inbound::Frame(Box::new(frame))),
                        Err(e) => {
                            tracing::warn!(error = %e, "undecodable frame from server");
                            return Some(Inbound::Undecodable);
                        }
                    },
                    Some(Ok(WsMessage::Ping(_) | WsMessage::Pong(_))) => continue,
                    _ => return None,
                }
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// A live client connection. Commands go out via [`send`](Self::send)
/// (sync, unbounded — the run loop never blocks on the wire); server
/// frames come back via [`next_frame`](Self::next_frame) /
/// [`try_next_frame`](Self::try_next_frame) for the run loop to drain.
#[derive(Debug)]
pub struct RemoteClient {
    cmd_tx: mpsc::UnboundedSender<Frame>,
    frames: mpsc::UnboundedReceiver<Inbound>,
    next_id: u64,
    undecodable: UndecodableFrames,
}

impl RemoteClient {
    /// Connect and handshake. Target: `"unix"` (the default socket),
    /// `"unix:<path>"`, or `"ws://host:port"`.
    pub async fn connect(target: &str) -> Result<Self> {
        // The token is stripped from the target here, so every message
        // below (dial errors, handshake errors) shows the clean URL only.
        let (target, token) = resolve_device_token(target, |key| std::env::var(key).ok());
        let target = target.as_str();
        let mut link = Link::open(target).await?;
        link.send(&client_hello(token)).await?;
        // Undecodable frames before the welcome are skipped, as before.
        let welcome = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
            loop {
                match link.recv().await {
                    Some(Inbound::Frame(frame)) => break Some(*frame),
                    Some(Inbound::Undecodable) => continue,
                    None => break None,
                }
            }
        })
        .await
        .map_err(|_| anyhow!("handshake timeout ({target})"))?;
        modron_protocol::client::check_welcome(welcome, target).map_err(anyhow::Error::msg)?;

        // Pump: owns the link, forwards both directions. The run loop
        // drains `frames` on its 50ms tick; send is sync/unbounded.
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<Frame>();
        let (frame_tx, frames) = mpsc::unbounded_channel::<Inbound>();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    cmd = cmd_rx.recv() => {
                        match cmd {
                            Some(frame) => {
                                if link.send(&frame).await.is_err() {
                                    break;
                                }
                            }
                            None => break,
                        }
                    }
                    inbound = link.recv() => {
                        match inbound {
                            Some(inbound) => {
                                if frame_tx.send(inbound).is_err() {
                                    break;
                                }
                            }
                            None => break,
                        }
                    }
                }
            }
        });

        Ok(Self {
            cmd_tx,
            frames,
            next_id: 2,
            undecodable: UndecodableFrames::default(),
        })
    }

    /// Send a command, returning its correlation id.
    pub fn send(&mut self, payload: Payload) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let _ = self.cmd_tx.send(Frame::request(id, payload));
        id
    }

    /// Next server item (awaiting). `Undecodable` is returned as it arrives
    /// so the caller can surface it; it is also counted on this connection.
    pub async fn next_frame(&mut self) -> Option<Inbound> {
        let inbound = self.frames.recv().await?;
        Some(self.observe(inbound))
    }

    /// Next server item if one arrived since the last tick.
    pub fn try_next_frame(&mut self) -> Option<Inbound> {
        let inbound = self.frames.try_recv().ok()?;
        Some(self.observe(inbound))
    }

    /// True exactly once per connection, the first time the server sent a
    /// frame this client could not decode. The run loop shows
    /// [`UNDECODABLE_NOTICE`] when it returns true.
    pub fn take_undecodable_notice(&mut self) -> bool {
        self.undecodable.take_notice()
    }

    /// Next decoded frame for the request loops below; undecodable items
    /// are counted and skipped (they have no UI of their own here).
    async fn recv_frame(&mut self) -> Option<Frame> {
        loop {
            match self.frames.recv().await? {
                Inbound::Frame(frame) => return Some(*frame),
                Inbound::Undecodable => self.undecodable.record(),
            }
        }
    }

    fn observe(&mut self, inbound: Inbound) -> Inbound {
        if matches!(inbound, Inbound::Undecodable) {
            self.undecodable.record();
        }
        inbound
    }

    /// List sessions on the server (`--resume` bare resolves newest).
    pub async fn list_sessions(&mut self) -> Result<Vec<modron_protocol::SessionMetaWire>> {
        let _id = self.send(Payload::ListSessions {});
        let deadline = tokio::time::sleep(REQUEST_TIMEOUT);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => bail!("list_sessions timeout"),
                frame = self.recv_frame() => match frame {
                    Some(Frame {
                        payload: Payload::Sessions { sessions },
                        ..
                    }) => return Ok(sessions),
                    Some(Frame {
                        payload: Payload::Error { message, kind: _ },
                        ..
                    }) => bail!("list_sessions failed: {message}"),
                    Some(_) => continue,
                    None => bail!("connection closed while listing sessions"),
                }
            }
        }
    }

    /// Attach a session: send `attach {session, last_seq}` and await the
    /// `snapshot` reply (skipping unrelated interleaved frames).
    pub async fn attach(&mut self, session: Uuid, last_seq: u64) -> Result<AttachSnapshot> {
        let id = self.next_id;
        self.next_id += 1;
        let _ = self.cmd_tx.send(Frame {
            id: Some(id),
            session: Some(session),
            seq: 0,
            payload: Payload::Attach {
                session_id: session,
                last_seq,
            },
        });
        let deadline = tokio::time::sleep(REQUEST_TIMEOUT);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => bail!("attach timeout waiting for snapshot"),
                frame = self.recv_frame() => match frame {
                    // Correlate by REQUEST id and session: two overlapping
                    // attaches (session switch mid-snapshot) must not adopt
                    // each other's snapshot (cycle-2 audit L6).
                    Some(Frame {
                        id: Some(frame_id),
                        payload:
                            Payload::Snapshot {
                                session_id,
                                last_seq,
                                events,
                                live,
                            },
                        ..
                    }) if frame_id == id && session_id == session => {
                        return Ok(AttachSnapshot {
                            session_id,
                            last_seq,
                            events,
                            live,
                        })
                    }
                    Some(Frame {
                        payload: Payload::Error { message, kind: _ },
                        ..
                    }) => bail!("attach failed: {message}"),
                    Some(_) => continue,
                    None => bail!("connection closed while attaching"),
                }
            }
        }
    }

    /// Whether the pump is gone (peer closed) — the run loop checks this
    /// to fall back to in-process mode or surface the disconnect.
    pub fn is_closed(&self) -> bool {
        self.cmd_tx.is_closed()
    }
}

// ---------------------------------------------------------------------------
// Tests: fixture servers speaking the protocol over both transports
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use modron_core::Usage;
    // Handshake now goes through modron_protocol::client, so the test
    // fixtures import the protocol constant directly.
    use modron_protocol::PROTOCOL_VERSION;
    use tokio::net::{TcpListener, UnixListener};

    /// A scripted Unix-socket server: reads hello, writes welcome; reads
    /// attach, writes a snapshot with one Task event.
    async fn unix_fixture(dir: &std::path::Path) -> PathBuf {
        let path = dir.join("test.sock");
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            let hello = lines.next_line().await.unwrap().unwrap();
            let hello: Frame = serde_json::from_str(&hello).unwrap();
            assert!(matches!(hello.payload, Payload::Hello { .. }));
            let welcome = Frame::request(
                hello.id.unwrap(),
                Payload::Welcome {
                    server_version: "test".into(),
                    protocol: PROTOCOL_VERSION,
                },
            );
            write
                .write_all(format!("{}\n", serde_json::to_string(&welcome).unwrap()).as_bytes())
                .await
                .unwrap();
            let attach = lines.next_line().await.unwrap().unwrap();
            let attach: Frame = serde_json::from_str(&attach).unwrap();
            match attach.payload {
                Payload::Attach { session_id, .. } => {
                    let snap = Frame {
                        id: Some(attach.id.unwrap()),
                        session: Some(session_id),
                        seq: 3,
                        payload: Payload::Snapshot {
                            session_id,
                            last_seq: 3,
                            events: vec![SessionEvent::Task {
                                text: "fixture".into(),
                            }],
                            live: LiveState {
                                running: true,
                                model: "m1".into(),
                                cumulative_usage: Usage {
                                    input_tokens: 5,
                                    output_tokens: 1,
                                    cached_input_tokens: 0,
                                },
                                ..LiveState::default()
                            },
                        },
                    };
                    write
                        .write_all(
                            format!("{}\n", serde_json::to_string(&snap).unwrap()).as_bytes(),
                        )
                        .await
                        .unwrap();
                }
                other => panic!("expected attach, got {other:?}"),
            }
            // Keep the socket open until the client drops.
            let _ = lines.next_line().await;
        });
        path
    }

    /// A scripted WS server: hello -> welcome, ping -> pong.
    async fn ws_fixture() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(WsMessage::Text(text))) = ws.next().await {
                let frame: Frame = serde_json::from_str(text.as_str()).unwrap();
                match frame.payload {
                    Payload::Hello { .. } => {
                        let welcome = Frame::request(
                            frame.id.unwrap(),
                            Payload::Welcome {
                                server_version: "test".into(),
                                protocol: PROTOCOL_VERSION,
                            },
                        );
                        ws.send(WsMessage::Text(
                            serde_json::to_string(&welcome).unwrap().into(),
                        ))
                        .await
                        .unwrap();
                    }
                    Payload::Ping {} => {
                        let pong = Frame::request(frame.id.unwrap(), Payload::Pong {});
                        ws.send(WsMessage::Text(
                            serde_json::to_string(&pong).unwrap().into(),
                        ))
                        .await
                        .unwrap();
                        break;
                    }
                    _ => break,
                }
            }
        });
        format!("ws://127.0.0.1:{port}")
    }

    #[tokio::test]
    async fn unix_connect_attach_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = unix_fixture(dir.path()).await;
        let mut client = RemoteClient::connect(&format!("unix:{}", path.display()))
            .await
            .unwrap();
        let session = Uuid::new_v4();
        let snap = client.attach(session, 0).await.unwrap();
        assert_eq!(snap.last_seq, 3);
        assert_eq!(snap.events.len(), 1);
        assert!(snap.live.running);
        assert_eq!(snap.live.model, "m1");
    }

    #[tokio::test]
    async fn ws_connect_ping_pong() {
        let url = ws_fixture().await;
        let mut client = RemoteClient::connect(&url).await.unwrap();
        let id = client.send(Payload::Ping {});
        loop {
            let Inbound::Frame(frame) = client.next_frame().await.unwrap() else {
                panic!("the ws fixture sends only decodable frames");
            };
            if frame.id == Some(id) {
                assert!(matches!(frame.payload, Payload::Pong {}));
                break;
            }
        }
    }

    #[test]
    fn undecodable_notice_is_raised_once_per_connection() {
        let mut counter = UndecodableFrames::default();
        assert!(!counter.take_notice(), "nothing undecodable yet");
        counter.record();
        assert_eq!(counter.count(), 1);
        assert!(counter.take_notice(), "the first one is announced");
        counter.record();
        counter.record();
        assert_eq!(counter.count(), 3, "every undecodable frame is counted");
        assert!(!counter.take_notice(), "later ones do not re-announce");
    }

    #[tokio::test]
    async fn an_undecodable_frame_is_surfaced_and_counted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("junk.sock");
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            let hello = lines.next_line().await.unwrap().unwrap();
            let hello: Frame = serde_json::from_str(&hello).unwrap();
            let welcome = Frame::request(
                hello.id.unwrap(),
                Payload::Welcome {
                    server_version: "test".into(),
                    protocol: PROTOCOL_VERSION,
                },
            );
            let note = Frame::request(
                9,
                Payload::Note {
                    text: "after junk".into(),
                },
            );
            let script = format!(
                "{}\n{{\"from_the_future\": true}}\nnot json at all\n{}\n",
                serde_json::to_string(&welcome).unwrap(),
                serde_json::to_string(&note).unwrap(),
            );
            write.write_all(script.as_bytes()).await.unwrap();
            let _ = lines.next_line().await;
        });
        let mut client = RemoteClient::connect(&format!("unix:{}", path.display()))
            .await
            .unwrap();
        assert!(!client.take_undecodable_notice(), "nothing seen yet");
        assert!(matches!(
            client.next_frame().await.unwrap(),
            Inbound::Undecodable
        ));
        assert!(client.take_undecodable_notice(), "announced on first sight");
        assert!(matches!(
            client.next_frame().await.unwrap(),
            Inbound::Undecodable
        ));
        let Inbound::Frame(frame) = client.next_frame().await.unwrap() else {
            panic!("the valid frame after the junk arrives");
        };
        assert!(matches!(frame.payload, Payload::Note { .. }));
        assert_eq!(client.undecodable.count(), 2);
        assert!(!client.take_undecodable_notice(), "once per connection");
    }

    #[tokio::test]
    async fn protocol_mismatch_is_refused() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            if let Some(Ok(WsMessage::Text(text))) = ws.next().await {
                let frame: Frame = serde_json::from_str(text.as_str()).unwrap();
                let welcome = Frame::request(
                    frame.id.unwrap(),
                    Payload::Welcome {
                        server_version: "test".into(),
                        protocol: PROTOCOL_VERSION + 1,
                    },
                );
                let _ = ws
                    .send(WsMessage::Text(
                        serde_json::to_string(&welcome).unwrap().into(),
                    ))
                    .await;
            }
        });
        let err = match RemoteClient::connect(&format!("ws://127.0.0.1:{port}")).await {
            Ok(_) => panic!("expected a protocol mismatch refusal"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("protocol"), "{err}");
    }

    #[tokio::test]
    async fn missing_socket_is_an_actionable_error() {
        let dir = tempfile::tempdir().unwrap();
        let ghost = dir.path().join("nothing.sock");
        let err = match RemoteClient::connect(&format!("unix:{}", ghost.display())).await {
            Ok(_) => panic!("expected a connect error"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("no server at"), "{err}");
    }

    #[test]
    fn token_param_is_stripped_from_the_target() {
        let (target, token) = split_token_param("ws://127.0.0.1:7699/?token=abc");
        assert_eq!(target, "ws://127.0.0.1:7699/");
        assert_eq!(token.as_deref(), Some("abc"));
    }

    #[test]
    fn token_param_keeps_other_query_params() {
        let (target, token) = split_token_param("wss://h.example/x?a=1&token=abc&b=2");
        assert_eq!(target, "wss://h.example/x?a=1&b=2");
        assert_eq!(token.as_deref(), Some("abc"));
    }

    #[test]
    fn empty_token_param_is_stripped_but_yields_no_token() {
        let (target, token) = split_token_param("ws://h:7699/?token=");
        assert_eq!(target, "ws://h:7699/");
        assert_eq!(token, None);
    }

    #[test]
    fn target_without_token_is_unchanged() {
        let (target, token) = split_token_param("ws://h:7699");
        assert_eq!(target, "ws://h:7699");
        assert_eq!(token, None);
        // Unix targets are never query-parsed.
        let (unix, token) = split_token_param("unix:/tmp/a?token=x.sock");
        assert_eq!(unix, "unix:/tmp/a?token=x.sock");
        assert_eq!(token, None);
    }

    #[test]
    fn url_token_beats_env_and_env_is_the_fallback() {
        let env = |key: &str| {
            assert_eq!(key, TOKEN_ENV);
            Some("from-env".to_string())
        };
        let (target, token) = resolve_device_token("ws://h:7699/?token=from-url", env);
        assert_eq!(target, "ws://h:7699/");
        assert_eq!(token.as_deref(), Some("from-url"));

        let (target, token) = resolve_device_token("ws://h:7699", env);
        assert_eq!(target, "ws://h:7699");
        assert_eq!(token.as_deref(), Some("from-env"));
    }

    #[test]
    fn no_url_token_and_no_env_means_no_token() {
        let (_, token) = resolve_device_token("ws://h:7699", |_| None);
        assert_eq!(token, None);
        let (_, token) = resolve_device_token("ws://h:7699", |_| Some(String::new()));
        assert_eq!(token, None);
    }

    #[test]
    fn hello_frame_carries_the_token() {
        let frame = client_hello(Some("tok-123".into()));
        match frame.payload {
            Payload::Hello { token, .. } => assert_eq!(token.as_deref(), Some("tok-123")),
            other => panic!("expected hello, got {other:?}"),
        }
        let frame = client_hello(None);
        assert!(matches!(frame.payload, Payload::Hello { token: None, .. }));
    }

    /// A WS server that records the request URI (path + query) and the
    /// token in the hello frame, then welcomes the client.
    // The handshake callback's error type (ErrorResponse) is large by
    // design in tungstenite; the fixture never returns an error.
    #[allow(clippy::result_large_err)]
    async fn ws_token_fixture() -> (
        String,
        tokio::sync::oneshot::Receiver<(String, Option<String>)>,
    ) {
        use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (uri_tx, uri_rx) = tokio::sync::oneshot::channel::<String>();
            let mut uri_tx = Some(uri_tx);
            let mut ws = tokio_tungstenite::accept_hdr_async(
                stream,
                move |req: &Request, resp: Response| {
                    if let Some(tx) = uri_tx.take() {
                        let _ = tx.send(req.uri().to_string());
                    }
                    Ok(resp)
                },
            )
            .await
            .unwrap();
            let hello_text = match ws.next().await {
                Some(Ok(WsMessage::Text(text))) => text,
                other => panic!("expected a hello text frame, got {other:?}"),
            };
            let hello: Frame = serde_json::from_str(hello_text.as_str()).unwrap();
            let token = match hello.payload {
                Payload::Hello { token, .. } => token,
                other => panic!("expected hello, got {other:?}"),
            };
            let welcome = Frame::request(
                hello.id.unwrap(),
                Payload::Welcome {
                    server_version: "test".into(),
                    protocol: PROTOCOL_VERSION,
                },
            );
            ws.send(WsMessage::Text(
                serde_json::to_string(&welcome).unwrap().into(),
            ))
            .await
            .unwrap();
            let uri = uri_rx.await.unwrap();
            let _ = done_tx.send((uri, token));
            // Hold the socket open until the client drops.
            let _ = ws.next().await;
        });
        (format!("ws://127.0.0.1:{port}"), done_rx)
    }

    #[tokio::test]
    async fn ws_token_is_sent_in_hello_and_never_dialed() {
        let (url, seen) = ws_token_fixture().await;
        let target = format!("{url}/?token=abc-123");
        let _client = RemoteClient::connect(&target).await.unwrap();
        let (uri, token) = seen.await.unwrap();
        assert_eq!(token.as_deref(), Some("abc-123"));
        assert!(!uri.contains("token"), "query leaked to the server: {uri}");
    }
}
