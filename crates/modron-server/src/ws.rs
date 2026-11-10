//! The remote WebSocket listener (Phase 2): one JSON [`Frame`] per WS text
//! message on a TCP listener (default `127.0.0.1:7699`). A listener bound
//! to a loopback address trusts its peers; anything else requires a paired
//! device token in the `hello` frame (TLS terminates at the tailscale /
//! cloudflared sidecar in front of it).

use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use modron_protocol::Frame;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::dispatch::{handle_frame, send, ConnCtx, Flow, ServerState};

/// Accept loop for one bound WebSocket listener. `trusted` reflects the
/// bind address (loopback = trusted).
pub async fn run_ws_listener(listener: TcpListener, state: Arc<ServerState>, trusted: bool) {
    loop {
        let Ok((stream, peer)) = listener.accept().await else {
            continue;
        };
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(e) = serve_ws_connection(stream, state, trusted).await {
                tracing::debug!(peer = %peer, error = %e, "websocket connection ended");
            }
        });
    }
}

async fn serve_ws_connection(
    stream: TcpStream,
    state: Arc<ServerState>,
    trusted: bool,
) -> anyhow::Result<()> {
    let ws = tokio_tungstenite::accept_async(stream).await?;
    let (mut sink, mut source) = ws.split();

    // Single writer: dispatch replies and session fan-out frames queue
    // through this channel in order.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Frame>();
    let writer = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            let Ok(json) = serde_json::to_string(&frame) else {
                break;
            };
            if sink.send(Message::text(json)).await.is_err() {
                break;
            }
        }
    });

    let mut ctx = ConnCtx::new(out_tx.clone());
    let mut flow = Flow::Continue;
    while flow == Flow::Continue {
        let Some(message) = source.next().await else {
            break;
        };
        let text = match message {
            Ok(Message::Text(text)) => text.to_string(),
            Ok(Message::Binary(bytes)) => String::from_utf8_lossy(&bytes).into_owned(),
            Ok(Message::Close(_)) => break,
            // Protocol-level pings/pongs are tungstenite's job.
            Ok(_) => continue,
            Err(_) => break,
        };
        match serde_json::from_str::<Frame>(&text) {
            Ok(frame) => flow = handle_frame(&state, &mut ctx, trusted, frame).await,
            Err(e) => {
                send(
                    &ctx,
                    Frame {
                        id: None,
                        session: None,
                        seq: 0,
                        payload: modron_protocol::Payload::Error {
                            message: format!("unparseable frame: {e}"),
                        },
                    },
                );
            }
        }
    }

    // Dropping ctx aborts the session fan-out pumps; the writer drains
    // what is already queued and exits when the channel closes.
    drop(ctx);
    drop(out_tx);
    let _ = writer.await;
    Ok(())
}
