//! The remote WebSocket listener (Phase 2): one JSON [`Frame`] per WS text
//! message on a TCP listener (default `127.0.0.1:7699`). A listener bound
//! to a loopback address trusts its peers; anything else requires a paired
//! device token in the `hello` frame (TLS terminates at the tailscale /
//! cloudflared sidecar in front of it). An upgrade that carries an `Origin`
//! header is a browser request: it is refused unless the origin is in the
//! explicit allowlist, so a web page cannot drive a loopback listener (D3).

use crate::dispatch::Outbound;
use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use modron_protocol::Frame;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::{Response as HttpResponse, StatusCode};
use tokio_tungstenite::tungstenite::Message;

use crate::dispatch::{handle_frame, send, ConnCtx, Flow, ServerState};

/// Accept loop for one bound WebSocket listener. `trusted` reflects the
/// bind address (loopback = trusted, and not `--require-token`).
/// `allowed_origins` are the exact `Origin` values a browser upgrade may
/// present; an upgrade with any other `Origin` is refused with 403.
pub async fn run_ws_listener(
    listener: TcpListener,
    state: Arc<ServerState>,
    trusted: bool,
    allowed_origins: Arc<Vec<String>>,
) {
    loop {
        let Ok((stream, peer)) = listener.accept().await else {
            continue;
        };
        let state = Arc::clone(&state);
        let allowed_origins = Arc::clone(&allowed_origins);
        tokio::spawn(async move {
            if let Err(e) = serve_ws_connection(stream, state, trusted, allowed_origins).await {
                tracing::debug!(peer = %peer, error = %e, "websocket connection ended");
            }
        });
    }
}

/// Decide whether an upgrade request may proceed. No `Origin` header means a
/// non-browser client (TUI, CLI, mobile shell) and is allowed; a present
/// `Origin` must match the allowlist exactly.
// The Err type is fixed by tungstenite's handshake Callback, not chosen here.
#[allow(clippy::result_large_err)]
fn check_origin(request: &Request, allowed_origins: &[String]) -> Result<(), ErrorResponse> {
    let Some(origin) = request.headers().get("origin") else {
        return Ok(());
    };
    match origin.to_str() {
        Ok(value) if allowed_origins.iter().any(|allowed| allowed == value) => Ok(()),
        _ => {
            tracing::debug!("websocket upgrade refused: origin not allowed");
            Err(HttpResponse::builder()
                .status(StatusCode::FORBIDDEN)
                .body(Some("origin not allowed".to_string()))
                .expect("static 403 response"))
        }
    }
}

async fn serve_ws_connection(
    stream: TcpStream,
    state: Arc<ServerState>,
    trusted: bool,
    allowed_origins: Arc<Vec<String>>,
) -> anyhow::Result<()> {
    // The default 64 MiB message cap is generous, but an explicit config
    // documents the dependency (a multi-MB snapshot rides one frame).
    // non_exhaustive: default-then-mutate (snapshot frames ride one WS
    // message; the default 64MiB cap rejects multi-MB sessions).
    let mut config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default();
    config.max_message_size = Some(256 * 1024 * 1024);
    config.max_frame_size = Some(16 * 1024 * 1024);
    #[allow(clippy::result_large_err)] // Err type fixed by tungstenite's Callback
    let origin_check = move |request: &Request, response: Response| {
        check_origin(request, &allowed_origins).map(|()| response)
    };
    let ws =
        tokio_tungstenite::accept_hdr_async_with_config(stream, origin_check, Some(config)).await?;
    let (mut sink, mut source) = ws.split();

    // Single writer: dispatch replies and session fan-out frames queue
    // through this channel in order.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Outbound>();
    let writer = tokio::spawn(async move {
        while let Some(outbound) = out_rx.recv().await {
            // Broadcasts arrive pre-serialized (one JSON build shared by
            // every viewer); replies serialize here.
            let json = match outbound {
                Outbound::Reply(frame) => serde_json::to_string(&frame),
                Outbound::Live(live) => Ok(live.json.to_string()),
            };
            let Ok(json) = json else { break };
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
                            kind: None,
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
