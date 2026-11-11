//! [`WireClient`] -- the WebSocket transport for the modron protocol.
//!
//! One JSON document per WebSocket message (the protocol's remote
//! framing). The client owns the send half; a reader task feeds inbound
//! frames into a channel. Handshake is `hello`/`welcome` with the
//! protocol-version check; requests carry monotonic ids and
//! [`WireClient::request`] correlates the reply, stashing any frames that
//! arrive in between for the next [`WireClient::recv`].

use std::collections::VecDeque;

use anyhow::{anyhow, Context as _};
use futures::{SinkExt, StreamExt};
use modron_core::Attachment;
use modron_protocol::{ConfigPatch, Decision, Frame, Payload, PROTOCOL_VERSION};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::{
    connect_async, tungstenite::Message as WsMessage, MaybeTlsStream, WebSocketStream,
};

/// The client name sent in `hello`.
pub const CLIENT_NAME: &str = "modron-desktop";

pub type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// A client-side protocol connection. Not `Clone`: one owner drives it
/// (the net loop).
pub struct WireClient {
    sink: futures::stream::SplitSink<WsStream, WsMessage>,
    inbound: mpsc::Receiver<Frame>,
    stash: VecDeque<Frame>,
    next_id: u64,
    last_seq: u64,
}

impl WireClient {
    /// Dial `url` and run the handshake: send `hello`, await `welcome`,
    /// fail unless the server speaks [`PROTOCOL_VERSION`]. Returns the
    /// client and the server's version string.
    pub async fn connect(url: &str) -> anyhow::Result<(Self, String)> {
        let (ws, _) = connect_async(url)
            .await
            .with_context(|| format!("dial {url}"))?;
        let (mut sink, mut stream) = ws.split();

        let hello = Frame::request(
            0,
            Payload::Hello {
                client: CLIENT_NAME.to_string(),
                protocol: PROTOCOL_VERSION,
                token: None,
            },
        );
        sink.send(WsMessage::text(serde_json::to_string(&hello)?))
            .await
            .context("send hello")?;

        // The first frame must be the welcome (skip nothing: a misbehaving
        // server that streams before the handshake fails loudly here).
        let msg = stream
            .next()
            .await
            .ok_or_else(|| anyhow!("connection closed before welcome"))?;
        let frame = decode(msg?).ok_or_else(|| anyhow!("non-text frame during handshake"))?;
        let server_version = match frame.payload {
            Payload::Welcome {
                server_version,
                protocol,
            } => {
                if protocol != PROTOCOL_VERSION {
                    return Err(anyhow!(
                        "protocol mismatch: server speaks {protocol}, we speak {PROTOCOL_VERSION}"
                    ));
                }
                server_version
            }
            Payload::Error { message } => return Err(anyhow!("handshake rejected: {message}")),
            other => {
                return Err(anyhow!(
                    "expected welcome, got {:?}",
                    std::mem::discriminant(&other)
                ))
            }
        };

        // Reader task: every subsequent inbound frame lands in the channel.
        let (tx, inbound) = mpsc::channel::<Frame>(256);
        tokio::spawn(async move {
            while let Some(msg) = stream.next().await {
                match msg {
                    Ok(m) => match decode(m) {
                        Some(frame) => {
                            if tx.send(frame).await.is_err() {
                                break;
                            }
                        }
                        None => continue,
                    },
                    Err(_) => break,
                }
            }
        });

        Ok((
            Self {
                sink,
                inbound,
                stash: VecDeque::new(),
                next_id: 1,
                last_seq: 0,
            },
            server_version,
        ))
    }

    /// The next request id (monotonic, never reused).
    pub fn next_request_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// The last server `seq` seen (the resume point for re-attach).
    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// Send one frame verbatim.
    pub async fn send_frame(&mut self, frame: Frame) -> anyhow::Result<()> {
        let json = serde_json::to_string(&frame)?;
        self.sink
            .send(WsMessage::text(json))
            .await
            .context("send frame")
    }

    /// Send a payload as a request (the client assigns the id).
    pub async fn send(&mut self, payload: Payload) -> anyhow::Result<u64> {
        let id = self.next_request_id();
        self.send_frame(Frame::request(id, payload)).await?;
        Ok(id)
    }

    /// Send a request and await its correlated reply. Non-matching frames
    /// that arrive meanwhile are stashed and returned by later
    /// [`WireClient::recv`] calls. An `error` reply becomes `Err`.
    pub async fn request(&mut self, payload: Payload) -> anyhow::Result<Frame> {
        let id = self.send(payload).await?;
        // Correlate on the live channel only: whatever arrives first that
        // is not this reply is stashed for the next recv(), never re-read
        // here (the stash stays the consumer-side replay queue).
        loop {
            let Some(frame) = self.inbound.recv().await else {
                return Err(anyhow!("closed"));
            };
            self.track_seq(&frame);
            if frame.id == Some(id) {
                if let Payload::Error { message } = frame.payload {
                    return Err(anyhow!("request {id} failed: {message}"));
                }
                return Ok(frame);
            }
            self.stash.push_back(frame);
        }
    }

    /// The next inbound frame: stashed frames first, then the socket.
    /// `None` when the connection closes.
    pub async fn recv(&mut self) -> Option<Frame> {
        if let Some(frame) = self.stash.pop_front() {
            self.track_seq(&frame);
            return Some(frame);
        }
        let frame = self.inbound.recv().await?;
        self.track_seq(&frame);
        Some(frame)
    }

    fn track_seq(&mut self, frame: &Frame) {
        if frame.seq > self.last_seq {
            self.last_seq = frame.seq;
        }
    }
}

/// Decode one WebSocket message into a [`Frame`]; non-text frames
/// (pings, binary, close) are skipped.
fn decode(msg: WsMessage) -> Option<Frame> {
    match msg {
        WsMessage::Text(text) => serde_json::from_str(text.as_str()).ok(),
        _ => None,
    }
}

/// What the net loop sends the UI thread.
#[derive(Debug)]
pub enum UiEvent {
    Frame(Box<Frame>),
    /// Handshake done; carries the server's version string.
    Connected(String),
    /// The socket dropped (or never came up) with the reason.
    Disconnected(String),
}

/// Commands the UI thread sends the net loop.
#[derive(Debug, Clone)]
pub enum Command {
    Attach(uuid::Uuid),
    Submit {
        session_id: Option<uuid::Uuid>,
        task: String,
        attachments: Vec<Attachment>,
    },
    Interrupt(uuid::Uuid),
    Approve {
        request_id: String,
        decision: Decision,
    },
    SetConfig(ConfigPatch),
    ListSessions,
    NewSession {
        title: Option<String>,
    },
    DeleteSession(uuid::Uuid),
    AuthProviders {},
    PersistRouter {
        cheap: String,
        strong: String,
        cheap_fallback: Option<String>,
        strong_fallback: Option<String>,
    },
    AutomationsList {},
    AutomationAdd {
        name: String,
        prompt: String,
        interval_minutes: u32,
    },
    AutomationRemove {
        name: String,
    },
    PublishPage {
        session_id: uuid::Uuid,
        title: String,
        content_md: String,
    },
    RemoteControl {
        session_id: uuid::Uuid,
        bridge_url: String,
    },
    CreateTask {
        title: String,
        base: Option<String>,
    },
    TaskStatus(uuid::Uuid),
    MergeTask {
        session_id: uuid::Uuid,
        message: Option<String>,
    },
    Ping,
}

/// Build the payload for a UI command (pure; unit-testable without a
/// socket). `last_seq` is the client's resume point for `attach`.
pub fn payload_for(command: &Command, last_seq: u64) -> Payload {
    match command {
        Command::Attach(id) => Payload::Attach {
            session_id: *id,
            last_seq,
        },
        Command::Submit {
            session_id,
            task,
            attachments,
        } => Payload::Submit {
            session_id: *session_id,
            task: task.clone(),
            attachments: attachments.clone(),
        },
        Command::Interrupt(id) => Payload::Interrupt { session_id: *id },
        Command::Approve {
            request_id,
            decision,
        } => Payload::Approve {
            request_id: request_id.clone(),
            decision: *decision,
        },
        Command::SetConfig(patch) => Payload::SetConfig {
            patch: patch.clone(),
        },
        Command::ListSessions => Payload::ListSessions {},
        Command::DeleteSession(id) => Payload::DeleteSession { session_id: *id },
        Command::AuthProviders {} => Payload::AuthProviders { rows: Vec::new() },
        Command::PersistRouter {
            cheap,
            strong,
            cheap_fallback,
            strong_fallback,
        } => Payload::PersistRouter {
            cheap: cheap.clone(),
            strong: strong.clone(),
            cheap_fallback: cheap_fallback.clone(),
            strong_fallback: strong_fallback.clone(),
        },
        Command::AutomationsList {} => Payload::AutomationsList {},
        Command::AutomationAdd {
            name,
            prompt,
            interval_minutes,
        } => Payload::AutomationAdd {
            name: name.clone(),
            prompt: prompt.clone(),
            interval_minutes: *interval_minutes,
        },
        Command::AutomationRemove { name } => Payload::AutomationRemove { name: name.clone() },
        Command::PublishPage {
            session_id,
            title,
            content_md,
        } => Payload::PublishPage {
            session_id: *session_id,
            title: title.clone(),
            content_md: content_md.clone(),
        },
        Command::RemoteControl {
            session_id,
            bridge_url,
        } => Payload::RemoteControl {
            session_id: *session_id,
            bridge_url: bridge_url.clone(),
        },
        Command::CreateTask { title, base } => Payload::CreateTask {
            title: title.clone(),
            base: base.clone(),
        },
        Command::TaskStatus(id) => Payload::GetTaskStatus { session_id: *id },
        Command::MergeTask {
            session_id,
            message,
        } => Payload::MergeTask {
            session_id: *session_id,
            message: message.clone(),
        },
        Command::NewSession { title } => Payload::NewSession {
            title: title.clone(),
        },
        Command::Ping => Payload::Ping {},
    }
}

/// The net loop: connect, then pump commands in and frames out until the
/// socket or the command channel dies. Reconnects are out of scope for
/// MVP -- the UI reports `Disconnected` and shows the offline state.
pub async fn run_client(
    url: String,
    cmd_rx: async_channel::Receiver<Command>,
    event_tx: async_channel::Sender<UiEvent>,
) {
    let mut client = match WireClient::connect(&url).await {
        Ok((c, server_version)) => {
            let _ = event_tx.send(UiEvent::Connected(server_version)).await;
            c
        }
        Err(e) => {
            let _ = event_tx.send(UiEvent::Disconnected(format!("{e:#}"))).await;
            return;
        }
    };

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                let Ok(cmd) = cmd else { break };
                let payload = payload_for(&cmd, client.last_seq());
                if let Err(e) = client.send(payload).await {
                    let _ = event_tx.send(UiEvent::Disconnected(format!("{e:#}"))).await;
                    break;
                }
            }
            frame = client.recv() => {
                let Some(frame) = frame else {
                    let _ = event_tx.send(UiEvent::Disconnected("connection closed".into())).await;
                    break;
                };
                if event_tx
                    .send(UiEvent::Frame(Box::new(frame)))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    }
}

/// Encode a frame as a WebSocket text message.
pub(crate) fn frame_message(frame: &Frame) -> WsMessage {
    WsMessage::text(serde_json::to_string(frame).expect("frame serializes"))
}

/// Parse an inbound WebSocket message as a frame.
pub(crate) fn parse_frame(msg: &WsMessage) -> Option<Frame> {
    match msg {
        WsMessage::Text(t) => serde_json::from_str(t.as_str()).ok(),
        _ => None,
    }
}

/// Bind a fresh loopback listener (the demo server and tests share it).
pub(crate) async fn bind_loopback() -> anyhow::Result<tokio::net::TcpListener> {
    Ok(tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?)
}

/// Accept one WebSocket connection from `listener` (plain, no TLS -- the
/// remote front is TLS-terminating).
pub(crate) async fn accept_ws(listener: &mut tokio::net::TcpListener) -> anyhow::Result<WsStream> {
    let (stream, _) = listener.accept().await?;
    Ok(tokio_tungstenite::accept_async(MaybeTlsStream::Plain(stream)).await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream::SplitSink;
    use modron_protocol::WireDelta;
    use uuid::Uuid;

    type TestSink = SplitSink<WsStream, WsMessage>;

    /// A scripted in-process WebSocket server: binds 127.0.0.1:0, accepts
    /// one connection, hands the script the split halves.
    struct TestServer {
        url: String,
    }

    impl TestServer {
        async fn start<F, Fut>(script: F) -> Self
        where
            F: FnOnce(TestSink, futures::stream::SplitStream<WsStream>) -> Fut + Send + 'static,
            Fut: std::future::Future<Output = ()> + Send + 'static,
        {
            let listener = bind_loopback().await.expect("bind loopback");
            let port = listener.local_addr().expect("local addr").port();
            // Accept inside the task: `start` returns before any client
            // dials, so awaiting the accept here would deadlock the test.
            tokio::spawn(async move {
                let mut listener = listener;
                let ws = accept_ws(&mut listener).await.expect("accept");
                let (sink, stream) = ws.split();
                script(sink, stream).await;
            });
            Self {
                url: format!("ws://127.0.0.1:{port}"),
            }
        }
    }

    async fn read_frame(stream: &mut futures::stream::SplitStream<WsStream>) -> Frame {
        loop {
            let msg = stream
                .next()
                .await
                .expect("server stream open")
                .expect("server read");
            if let Some(frame) = parse_frame(&msg) {
                return frame;
            }
        }
    }

    async fn write_frame(sink: &mut TestSink, frame: Frame) {
        sink.send(frame_message(&frame))
            .await
            .expect("server write");
    }

    /// Read the next client request, skipping the handshake `hello` the
    /// client sends before any test request.
    async fn read_request(stream: &mut futures::stream::SplitStream<WsStream>) -> Frame {
        loop {
            let frame = read_frame(stream).await;
            if !matches!(frame.payload, Payload::Hello { .. }) {
                return frame;
            }
        }
    }

    /// The scripted welcome: every test's server replies to `hello` first.
    async fn welcome(sink: &mut TestSink) {
        write_frame(
            sink,
            Frame::event(
                None,
                0,
                Payload::Welcome {
                    server_version: "test/1".into(),
                    protocol: PROTOCOL_VERSION,
                },
            ),
        )
        .await;
    }

    #[tokio::test]
    async fn handshake_and_ping_roundtrip() {
        let server = TestServer::start(|mut sink, mut stream| async move {
            welcome(&mut sink).await;
            // Answer the first ping; ignore anything else.
            while let Ok(frame) =
                tokio::time::timeout(std::time::Duration::from_secs(5), read_frame(&mut stream))
                    .await
            {
                if let Payload::Ping { .. } = frame.payload {
                    let mut pong = Frame::event(None, 0, Payload::Pong {});
                    pong.id = frame.id;
                    write_frame(&mut sink, pong).await;
                    return;
                }
            }
        })
        .await;

        let (mut client, version) = WireClient::connect(&server.url).await.expect("connect");
        assert_eq!(version, "test/1");

        let reply = client.request(Payload::Ping {}).await.expect("request");
        // The request id was assigned monotonically (first is 1) and the
        // pong echoes it back.
        assert_eq!(reply.id, Some(1));
        assert!(matches!(reply.payload, Payload::Pong {}));
    }

    #[tokio::test]
    async fn protocol_mismatch_is_rejected() {
        let server = TestServer::start(|mut sink, _stream| async move {
            write_frame(
                &mut sink,
                Frame::event(
                    None,
                    0,
                    Payload::Welcome {
                        server_version: "old".into(),
                        protocol: PROTOCOL_VERSION + 1,
                    },
                ),
            )
            .await;
        })
        .await;
        let Err(err) = WireClient::connect(&server.url).await else {
            panic!("protocol mismatch must fail");
        };
        assert!(err.to_string().contains("protocol mismatch"), "{err}");
    }

    #[tokio::test]
    async fn seq_tracking_follows_event_frames() {
        let session = Uuid::new_v4();
        let server = TestServer::start(move |mut sink, _stream| async move {
            welcome(&mut sink).await;
            for seq in 1..=3u64 {
                write_frame(
                    &mut sink,
                    Frame::event(
                        Some(session),
                        seq,
                        Payload::Delta {
                            delta: WireDelta::Text {
                                text: format!("chunk{seq}"),
                            },
                        },
                    ),
                )
                .await;
            }
            // Keep the socket alive briefly so the client drains all three.
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        })
        .await;

        let (mut client, _) = WireClient::connect(&server.url).await.expect("connect");
        let mut texts = Vec::new();
        for _ in 0..3 {
            let frame = client.recv().await.expect("frame");
            match frame.payload {
                Payload::Delta { delta } => {
                    if let WireDelta::Text { text } = delta {
                        texts.push(text);
                    }
                }
                other => panic!("expected delta, got {other:?}"),
            }
        }
        assert_eq!(texts, vec!["chunk1", "chunk2", "chunk3"]);
        assert_eq!(client.last_seq(), 3);
    }

    #[tokio::test]
    async fn request_correlation_stashes_interleaved_frames() {
        let session = Uuid::new_v4();
        let server = TestServer::start(move |mut sink, mut stream| async move {
            welcome(&mut sink).await;
            let ping = read_request(&mut stream).await;
            // Interleave a server event BEFORE the correlated reply: the
            // client must stash it, not consume it as the reply.
            write_frame(
                &mut sink,
                Frame::event(
                    Some(session),
                    1,
                    Payload::Note {
                        text: "meanwhile".into(),
                    },
                ),
            )
            .await;
            let mut pong = Frame::event(None, 0, Payload::Pong {});
            pong.id = ping.id;
            write_frame(&mut sink, pong).await;
        })
        .await;

        let (mut client, _) = WireClient::connect(&server.url).await.expect("connect");
        let reply = client.request(Payload::Ping {}).await.expect("correlated");
        assert!(matches!(reply.payload, Payload::Pong {}));
        // The stashed event comes through next, in order.
        let stashed = client.recv().await.expect("stashed frame");
        assert!(matches!(stashed.payload, Payload::Note { .. }));
    }

    #[tokio::test]
    async fn error_reply_becomes_a_request_failure() {
        let server = TestServer::start(|mut sink, mut stream| async move {
            welcome(&mut sink).await;
            let frame = read_request(&mut stream).await;
            let mut err = Frame::event(
                None,
                0,
                Payload::Error {
                    message: "no such session".into(),
                },
            );
            err.id = frame.id;
            write_frame(&mut sink, err).await;
            // Hold the socket open briefly so the reply lands before the
            // close (a real server does not drop mid-request).
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        })
        .await;

        let (mut client, _) = WireClient::connect(&server.url).await.expect("connect");
        let err = client
            .request(Payload::ListSessions {})
            .await
            .expect_err("error payload maps to Err");
        assert!(err.to_string().contains("no such session"), "{err}");
    }

    #[tokio::test]
    async fn payload_for_builds_protocol_shapes() {
        let session = Uuid::new_v4();
        let attach = payload_for(&Command::Attach(session), 42);
        assert!(matches!(
            attach,
            Payload::Attach {
                session_id,
                last_seq: 42,
            } if session_id == session
        ));
        let attachments = vec![modron_core::Attachment::Image {
            media_type: "image/png".into(),
            data: "aGk=".into(),
        }];
        let submit = payload_for(
            &Command::Submit {
                session_id: Some(session),
                task: "fix it".into(),
                attachments: attachments.clone(),
            },
            0,
        );
        assert!(matches!(
            &submit,
            Payload::Submit {
                session_id: Some(s),
                task,
                attachments: got
            } if *s == session
                && task == "fix it"
                && got.len() == 1
                && matches!(&got[0], Attachment::Image { media_type, .. } if media_type == "image/png")
        ));
        let cfg = payload_for(
            &Command::SetConfig(ConfigPatch {
                model: Some("m".into()),
                ..ConfigPatch::default()
            }),
            0,
        );
        assert!(matches!(
            cfg,
            Payload::SetConfig { patch } if patch.model.as_deref() == Some("m")
        ));
    }
}
