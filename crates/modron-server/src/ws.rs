//! The remote WebSocket listener (Phase 2): one JSON [`Frame`] per WS text
//! message on a TCP listener (default `127.0.0.1:7699`). A listener bound
//! to a loopback address trusts its peers; anything else requires a paired
//! device token in the `hello` frame (TLS terminates at the tailscale /
//! cloudflared sidecar in front of it). An upgrade that carries an `Origin`
//! header is a browser request: it is refused unless the origin is in the
//! explicit allowlist, so a web page cannot drive a loopback listener (D3).
//!
//! Pre-auth bounds (security plan B4): until `hello` is accepted, a message
//! larger than [`WsLimits::pre_auth_max_bytes`] closes the connection, and
//! the whole connection must reach `hello` within
//! [`WsLimits::hello_deadline`] of the TCP accept. A client that stops
//! reading is cut off when one outbound frame cannot be written within
//! [`WsLimits::slow_consumer_timeout`].

use crate::dispatch::Outbound;
use std::sync::Arc;
use std::time::Duration;

use futures::stream::SplitStream;
use futures::{SinkExt, StreamExt};
use modron_protocol::Frame;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::{Response as HttpResponse, StatusCode};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::dispatch::{handle_frame, send, ConnCtx, Flow, ServerState};

/// Pre-auth and back-pressure bounds for one WebSocket connection.
#[derive(Clone, Copy, Debug)]
pub(crate) struct WsLimits {
    /// Time from TCP accept until `hello` must have been accepted.
    pub hello_deadline: Duration,
    /// Largest message accepted before the connection is authenticated.
    pub pre_auth_max_bytes: usize,
    /// Longest a single outbound frame may take to be written.
    pub slow_consumer_timeout: Duration,
}

impl WsLimits {
    pub(crate) const DEFAULT: Self = Self {
        hello_deadline: Duration::from_secs(10),
        pre_auth_max_bytes: 64 * 1024,
        slow_consumer_timeout: Duration::from_secs(30),
    };
}

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
            let result =
                serve_ws_connection(stream, state, trusted, allowed_origins, WsLimits::DEFAULT)
                    .await;
            if let Err(e) = result {
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
    limits: WsLimits,
) -> anyhow::Result<()> {
    // The hello deadline runs from the TCP accept, so it also covers the
    // WebSocket upgrade itself.
    let hello_deadline = Instant::now() + limits.hello_deadline;

    // The default 64 MiB message cap is generous, but an explicit config
    // documents the dependency (a multi-MB snapshot rides one frame).
    // non_exhaustive: default-then-mutate (snapshot frames ride one WS
    // message; the default 64MiB cap rejects multi-MB sessions).
    // The pre-auth 64 KiB cap is enforced in the read loop, not here: the
    // tungstenite config is fixed at accept time, so the frame is buffered
    // by tungstenite before that check runs.
    let mut config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default();
    config.max_message_size = Some(256 * 1024 * 1024);
    config.max_frame_size = Some(16 * 1024 * 1024);
    #[allow(clippy::result_large_err)] // Err type fixed by tungstenite's Callback
    let origin_check = move |request: &Request, response: Response| {
        check_origin(request, &allowed_origins).map(|()| response)
    };
    let accept =
        tokio_tungstenite::accept_hdr_async_with_config(stream, origin_check, Some(config));
    let ws: WebSocketStream<TcpStream> = match tokio::time::timeout_at(hello_deadline, accept).await
    {
        Ok(ws) => ws?,
        Err(_) => anyhow::bail!("websocket upgrade exceeded the hello deadline"),
    };
    let (mut sink, mut source) = ws.split();

    // Single writer: dispatch replies and session fan-out frames queue
    // through this channel in order. A frame that cannot be written within
    // the slow-consumer timeout ends the writer, which ends the connection.
    let slow_consumer_timeout = limits.slow_consumer_timeout;
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Outbound>();
    let mut writer = tokio::spawn(async move {
        while let Some(outbound) = out_rx.recv().await {
            // Broadcasts arrive pre-serialized (one JSON build shared by
            // every viewer); replies serialize here.
            let json = match outbound {
                Outbound::Reply(frame) => serde_json::to_string(&frame),
                Outbound::Live(live) => Ok(live.json.to_string()),
            };
            let Ok(json) = json else { break };
            match tokio::time::timeout(slow_consumer_timeout, sink.send(Message::text(json))).await
            {
                Ok(Ok(())) => {}
                Ok(Err(_)) => break,
                Err(_) => {
                    tracing::debug!("websocket client too slow; closing connection");
                    break;
                }
            }
        }
    });

    let mut ctx = ConnCtx::new(out_tx.clone());
    let mut flow = Flow::Continue;
    let mut writer_done = false;
    while flow == Flow::Continue {
        let next = tokio::select! {
            biased;
            _ = &mut writer => {
                writer_done = true;
                break;
            }
            next = next_message(&mut source, ctx.authed, hello_deadline) => next,
        };
        let message = match next {
            Some(Ok(message)) => message,
            // Stream end, transport error, or no hello before the deadline.
            _ => break,
        };
        let text = match message {
            Message::Text(text) => {
                if !ctx.authed && text.len() > limits.pre_auth_max_bytes {
                    tracing::debug!(bytes = text.len(), "websocket: oversized pre-auth message");
                    break;
                }
                text.to_string()
            }
            Message::Binary(bytes) => {
                if !ctx.authed && bytes.len() > limits.pre_auth_max_bytes {
                    tracing::debug!(bytes = bytes.len(), "websocket: oversized pre-auth message");
                    break;
                }
                String::from_utf8_lossy(&bytes).into_owned()
            }
            Message::Close(_) => break,
            // Protocol-level pings/pongs are tungstenite's job.
            _ => continue,
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
    // what is already queued and exits when the channel closes. A writer
    // that already finished (slow consumer) is not awaited twice.
    drop(ctx);
    drop(out_tx);
    if !writer_done {
        let _ = writer.await;
    }
    Ok(())
}

/// Next inbound message. Before authentication the read is bounded by the
/// hello deadline; after it, reads wait as long as the client keeps the
/// connection open.
async fn next_message(
    source: &mut SplitStream<WebSocketStream<TcpStream>>,
    authed: bool,
    hello_deadline: Instant,
) -> Option<Result<Message, tokio_tungstenite::tungstenite::Error>> {
    if authed {
        return source.next().await;
    }
    match tokio::time::timeout_at(hello_deadline, source.next()).await {
        Ok(next) => next,
        Err(_) => {
            tracing::debug!("websocket: no hello before the deadline; closing");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use modron_protocol::Payload;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    fn test_state(dir: &std::path::Path) -> Arc<ServerState> {
        let store = modron_store::JsonlSessionStore::new(dir.join("sessions"));
        Arc::new(ServerState {
            hub: crate::hub::SessionHub::empty(),
            store: Arc::new(store),
            devices_dir: dir.to_path_buf(),
            version: "test".into(),
            working_dir: dir.to_path_buf(),
            tasks: crate::tasks::TaskRegistry::new(),
            bridge_base: None,
            rc_keys: std::sync::Mutex::new(Default::default()),
            rc_tunnels: std::sync::Mutex::new(Default::default()),
        })
    }

    /// Serve one accepted connection with `limits`; returns the listen
    /// address and the server task.
    async fn spawn_one(
        limits: WsLimits,
    ) -> (
        std::net::SocketAddr,
        tokio::task::JoinHandle<anyhow::Result<()>>,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve_ws_connection(stream, state, true, Arc::new(Vec::new()), limits).await
        });
        (addr, task, dir)
    }

    #[tokio::test]
    async fn silent_connection_is_dropped_at_hello_deadline() {
        let limits = WsLimits {
            hello_deadline: Duration::from_millis(300),
            ..WsLimits::DEFAULT
        };
        let (addr, task, _dir) = spawn_one(limits).await;
        // Complete the WebSocket upgrade, then send nothing.
        let (_client, _) = tokio_tungstenite::connect_async(format!("ws://{addr}"))
            .await
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("server must drop a silent connection within the deadline")
            .unwrap();
        assert!(result.is_ok(), "hello deadline ends the read loop cleanly");
    }

    #[tokio::test]
    async fn slow_consumer_is_cut_off() {
        let limits = WsLimits {
            slow_consumer_timeout: Duration::from_millis(200),
            ..WsLimits::DEFAULT
        };
        let (addr, task, _dir) = spawn_one(limits).await;
        let (mut client, _) = tokio_tungstenite::connect_async(format!("ws://{addr}"))
            .await
            .unwrap();
        let hello = Frame::request(
            1,
            Payload::Hello {
                client: "test".into(),
                protocol: modron_protocol::PROTOCOL_VERSION,
                token: None,
            },
        );
        client
            .send(WsMessage::text(serde_json::to_string(&hello).unwrap()))
            .await
            .unwrap();
        // Flood pings and never read the replies, so the server's writer
        // blocks on a full socket and must give up.
        let ping = serde_json::to_string(&Frame::request(2, Payload::Ping {})).unwrap();
        let flood = async {
            for _ in 0..200_000 {
                if client.send(WsMessage::text(ping.clone())).await.is_err() {
                    break;
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(15), flood)
            .await
            .ok();
        let result = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("server must cut off a client that stops reading")
            .unwrap();
        assert!(result.is_ok());
    }
}
