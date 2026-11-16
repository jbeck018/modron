//! Native transports: tokio-tungstenite over TCP for `ws://` and `wss://`,
//! and a Unix domain socket for `unix` targets (the terminal's local fast
//! path, same framing as crates/modron-tui/src/remote.rs: one JSON frame per
//! line). The WebSocket side answers protocol pings inline (tungstenite
//! surfaces them as messages; the server expects pongs) so the shared run
//! loop only ever sees text and close events.

use crate::client::{handshake_timeout, hello_frame, In};
use crate::error::ClientError;
use futures_util::{SinkExt, StreamExt};
use modron_protocol::{Frame, Payload, PROTOCOL_VERSION};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{connect_async_with_config, MaybeTlsStream, WebSocketStream};

/// Largest whole message accepted: the server sends a full snapshot in one
/// frame. Same limits as the desktop client (the retired desktop wire.rs), so a
/// session that opens on desktop opens here.
const MAX_MESSAGE_BYTES: usize = 256 * 1024 * 1024;
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

type WsStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// One side of an established connection: what the shared run loop
/// drives.
pub(crate) enum Conn {
    Ws {
        sink: futures_util::stream::SplitSink<WsStream, WsMessage>,
        stream: futures_util::stream::SplitStream<WsStream>,
    },
    #[cfg(unix)]
    Unix {
        write: tokio::net::unix::OwnedWriteHalf,
        lines: tokio::io::Lines<tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>>,
    },
}

impl Conn {
    pub(crate) async fn send_text(&mut self, json: &str) -> Result<(), ClientError> {
        match self {
            Self::Ws { sink, .. } => sink
                .send(WsMessage::Text(json.into()))
                .await
                .map_err(|e| ClientError::Handshake(e.to_string())),
            #[cfg(unix)]
            Self::Unix { write, .. } => {
                use tokio::io::AsyncWriteExt;
                let framed = format!("{json}\n");
                write
                    .write_all(framed.as_bytes())
                    .await
                    .map_err(|e| ClientError::Handshake(e.to_string()))
            }
        }
    }

    pub(crate) async fn close(&mut self) {
        match self {
            Self::Ws { sink, .. } => {
                let _ = sink.send(WsMessage::Close(None)).await;
            }
            #[cfg(unix)]
            Self::Unix { write, .. } => {
                use tokio::io::AsyncWriteExt;
                let _ = write.shutdown().await;
            }
        }
    }

    /// Next inbound event, answering pings inline. `None` = the socket
    /// closed cleanly.
    pub(crate) async fn next_in(&mut self) -> Option<Result<In, ClientError>> {
        match self {
            Self::Ws { sink, stream } => loop {
                match stream.next().await? {
                    Ok(WsMessage::Text(json)) => return Some(Ok(In::Text(json.to_string()))),
                    Ok(WsMessage::Ping(data)) => {
                        let _ = sink.send(WsMessage::Pong(data)).await;
                    }
                    Ok(WsMessage::Close(_)) => return Some(Ok(In::Closed)),
                    Ok(_) => {}
                    Err(e) => return Some(Err(ClientError::Handshake(e.to_string()))),
                }
            },
            #[cfg(unix)]
            Self::Unix { lines, .. } => loop {
                match lines.next_line().await {
                    Ok(Some(line)) if line.trim().is_empty() => continue,
                    Ok(Some(line)) => return Some(Ok(In::Text(line))),
                    Ok(None) => return None,
                    Err(e) => return Some(Err(ClientError::Handshake(e.to_string()))),
                }
            },
        }
    }
}

/// Connect (WebSocket or Unix socket, by target) and run the hello/welcome
/// handshake. The caller wraps this in the connect deadline. Returns the
/// connection ready for the run loop plus the server's reported version.
pub(crate) async fn handshake(
    url: &str,
    token: Option<&str>,
) -> Result<(Conn, String), ClientError> {
    match crate::client::unix_socket_path(url)? {
        Some(path) => unix_handshake(&path, token).await,
        None => ws_handshake(url, token).await,
    }
}

/// Map the server's first frame to its version, or the mismatch/refusal.
fn welcome_version(frame: Frame) -> Result<String, ClientError> {
    match frame.payload {
        Payload::Welcome {
            server_version,
            protocol,
        } if protocol == PROTOCOL_VERSION => Ok(server_version),
        Payload::Welcome { protocol, .. } => Err(ClientError::ProtocolMismatch {
            server: protocol,
            client: PROTOCOL_VERSION,
        }),
        other => Err(ClientError::Handshake(format!(
            "expected welcome, got {other:?}"
        ))),
    }
}

#[cfg(unix)]
async fn unix_handshake(
    path: &std::path::Path,
    token: Option<&str>,
) -> Result<(Conn, String), ClientError> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let stream = tokio::net::UnixStream::connect(path).await.map_err(|e| {
        ClientError::Connect(format!(
            "no server at {} ({e}); start one with: modron serve",
            path.display()
        ))
    })?;
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let json =
        serde_json::to_string(&hello_frame(token)).map_err(|e| ClientError::Json(e.to_string()))?;
    write
        .write_all(format!("{json}\n").as_bytes())
        .await
        .map_err(|e| ClientError::Handshake(e.to_string()))?;
    // Undecodable lines before the welcome are skipped, as the TUI does.
    loop {
        match lines.next_line().await {
            Ok(Some(line)) if line.trim().is_empty() => continue,
            Ok(Some(line)) => match serde_json::from_str::<Frame>(&line) {
                Ok(frame) => {
                    let server_version = welcome_version(frame)?;
                    return Ok((Conn::Unix { write, lines }, server_version));
                }
                Err(_) => continue,
            },
            Ok(None) => {
                return Err(ClientError::Handshake("closed during handshake".into()));
            }
            Err(e) => return Err(ClientError::Handshake(e.to_string())),
        }
    }
}

#[cfg(not(unix))]
async fn unix_handshake(
    _path: &std::path::Path,
    _token: Option<&str>,
) -> Result<(Conn, String), ClientError> {
    Err(ClientError::Connect(
        "unix sockets are not available on this platform".into(),
    ))
}

async fn ws_handshake(url: &str, token: Option<&str>) -> Result<(Conn, String), ClientError> {
    // Limits are set explicitly (the tungstenite defaults are 64 MiB per
    // message); `connect` itself is bounded by the caller's connect deadline.
    let mut config = WebSocketConfig::default();
    config.max_message_size = Some(MAX_MESSAGE_BYTES);
    config.max_frame_size = Some(MAX_FRAME_BYTES);
    let (ws, _) = connect_async_with_config(url, Some(config), false)
        .await
        .map_err(|e| ClientError::Connect(e.to_string()))?;
    let (mut sink, mut stream) = ws.split();
    let json =
        serde_json::to_string(&hello_frame(token)).map_err(|e| ClientError::Json(e.to_string()))?;
    sink.send(WsMessage::Text(json.into()))
        .await
        .map_err(|e| ClientError::Handshake(e.to_string()))?;
    let msg = crate::client::with_timeout(handshake_timeout(), stream.next())
        .await
        .ok_or(ClientError::Timeout)?;
    let frame = match msg {
        Some(Ok(WsMessage::Text(json))) => {
            serde_json::from_str::<Frame>(&json).map_err(|e| ClientError::Json(e.to_string()))?
        }
        Some(Ok(_)) => return Err(ClientError::Handshake("expected a welcome frame".into())),
        Some(Err(e)) => return Err(ClientError::Handshake(e.to_string())),
        None => return Err(ClientError::Handshake("closed during handshake".into())),
    };
    let server_version = welcome_version(frame)?;
    Ok((Conn::Ws { sink, stream }, server_version))
}
