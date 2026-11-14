//! Native WebSocket transport: tokio-tungstenite over TCP. The inbound
//! side answers protocol pings inline (tungstenite surfaces them as
//! messages; the server expects pongs) so the shared run loop only ever
//! sees text and close events.

use crate::client::{handshake_timeout, hello_frame, In};
use crate::error::ClientError;
use futures_util::{SinkExt, StreamExt};
use modron_protocol::{Frame, Payload, PROTOCOL_VERSION};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{connect_async_with_config, MaybeTlsStream, WebSocketStream};

/// Largest whole message accepted: the server sends a full snapshot in one
/// frame. Same limits as the desktop client (`modron-desktop` wire.rs), so a
/// session that opens on desktop opens here.
const MAX_MESSAGE_BYTES: usize = 256 * 1024 * 1024;
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

type WsStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// One side of an established connection: what the shared run loop
/// drives.
pub(crate) struct Conn {
    sink: futures_util::stream::SplitSink<WsStream, WsMessage>,
    stream: futures_util::stream::SplitStream<WsStream>,
}

impl Conn {
    pub(crate) async fn send_text(&mut self, json: &str) -> Result<(), ClientError> {
        self.sink
            .send(WsMessage::Text(json.into()))
            .await
            .map_err(|e| ClientError::Handshake(e.to_string()))
    }

    pub(crate) async fn close(&mut self) {
        let _ = self.sink.send(WsMessage::Close(None)).await;
    }

    /// Next inbound event, answering pings inline. `None` = the socket
    /// closed cleanly.
    pub(crate) async fn next_in(&mut self) -> Option<Result<In, ClientError>> {
        loop {
            match self.stream.next().await? {
                Ok(WsMessage::Text(json)) => return Some(Ok(In::Text(json.to_string()))),
                Ok(WsMessage::Ping(data)) => {
                    let _ = self.sink.send(WsMessage::Pong(data)).await;
                }
                Ok(WsMessage::Close(_)) => return Some(Ok(In::Closed)),
                Ok(_) => {}
                Err(e) => return Some(Err(ClientError::Handshake(e.to_string()))),
            }
        }
    }
}

/// TCP connect + hello/welcome handshake with a deadline. Returns the
/// connection ready for the run loop plus the server's reported version.
pub(crate) async fn handshake(
    url: &str,
    token: Option<&str>,
) -> Result<(Conn, String), ClientError> {
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
    match frame.payload {
        Payload::Welcome {
            server_version,
            protocol,
        } if protocol == PROTOCOL_VERSION => Ok((Conn { sink, stream }, server_version)),
        Payload::Welcome { protocol, .. } => Err(ClientError::ProtocolMismatch {
            server: protocol,
            client: PROTOCOL_VERSION,
        }),
        other => Err(ClientError::Handshake(format!(
            "expected welcome, got {other:?}"
        ))),
    }
}
