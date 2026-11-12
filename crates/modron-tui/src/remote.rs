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
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use uuid::Uuid;

/// Handshake deadline: a server that cannot answer Welcome in 5s is not a
/// modron server (or is wedged) — fail fast into the in-process fallback.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Reply deadline for a correlated request (attach).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

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
                    let (stream, _) = connect_async(target)
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

    /// Next frame, or None when the peer closed.
    async fn recv(&mut self) -> Option<Frame> {
        match self {
            Self::Unix { lines, .. } => loop {
                match lines.next_line().await {
                    Ok(Some(line)) if line.trim().is_empty() => continue,
                    Ok(Some(line)) => match serde_json::from_str(&line) {
                        Ok(frame) => return Some(frame),
                        Err(e) => {
                            tracing::warn!(error = %e, "dropping undecodable line from server");
                            continue;
                        }
                    },
                    _ => return None,
                }
            },
            Self::Ws { stream } => loop {
                match stream.next().await {
                    Some(Ok(WsMessage::Text(text))) => match serde_json::from_str(&text) {
                        Ok(frame) => return Some(frame),
                        Err(e) => {
                            tracing::warn!(error = %e, "dropping undecodable frame from server");
                            continue;
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
    frames: mpsc::UnboundedReceiver<Frame>,
    next_id: u64,
}

impl RemoteClient {
    /// Connect and handshake. Target: `"unix"` (the default socket),
    /// `"unix:<path>"`, or `"ws://host:port"`.
    pub async fn connect(target: &str) -> Result<Self> {
        let mut link = Link::open(target).await?;
        link.send(&modron_protocol::client::hello_frame("modron-tui", None))
            .await?;
        let welcome = tokio::time::timeout(HANDSHAKE_TIMEOUT, link.recv())
            .await
            .map_err(|_| anyhow!("handshake timeout ({target})"))?;
        modron_protocol::client::check_welcome(welcome, target).map_err(anyhow::Error::msg)?;

        // Pump: owns the link, forwards both directions. The run loop
        // drains `frames` on its 50ms tick; send is sync/unbounded.
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<Frame>();
        let (frame_tx, frames) = mpsc::unbounded_channel::<Frame>();
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
                    frame = link.recv() => {
                        match frame {
                            Some(frame) => {
                                if frame_tx.send(frame).is_err() {
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
        })
    }

    /// Send a command, returning its correlation id.
    pub fn send(&mut self, payload: Payload) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let _ = self.cmd_tx.send(Frame::request(id, payload));
        id
    }

    /// Next server frame (awaiting).
    pub async fn next_frame(&mut self) -> Option<Frame> {
        self.frames.recv().await
    }

    /// Next server frame if one arrived since the last tick.
    pub fn try_next_frame(&mut self) -> Option<Frame> {
        self.frames.try_recv().ok()
    }

    /// List sessions on the server (`--resume` bare resolves newest).
    pub async fn list_sessions(&mut self) -> Result<Vec<modron_protocol::SessionMetaWire>> {
        let _id = self.send(Payload::ListSessions {});
        let deadline = tokio::time::sleep(REQUEST_TIMEOUT);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => bail!("list_sessions timeout"),
                frame = self.frames.recv() => match frame {
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
                frame = self.frames.recv() => match frame {
                    Some(Frame {
                        payload:
                            Payload::Snapshot {
                                session_id,
                                last_seq,
                                events,
                                live,
                            },
                        ..
                    }) => {
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
            let frame = client.next_frame().await.unwrap();
            if frame.id == Some(id) {
                assert!(matches!(frame.payload, Payload::Pong {}));
                break;
            }
        }
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
}
