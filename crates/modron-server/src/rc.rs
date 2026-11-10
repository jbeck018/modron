//! rc.rs — remote-control tunnels (the `/remote-control` flow): one
//! outbound WebSocket from this server to a relay bridge (a deployed
//! cloud/bridge worker), keyed by a share code. The relay is
//! transport-only: it stores nothing, it relays Frames.
//!
//! Over the tunnel this server acts as a normal protocol connection: the
//! web/phone page on the other side of the relay sends ClientMsg frames
//! (hello, attach, submit, approve...) which flow through the SAME
//! `handle_frame` dispatch as every other client, and ServerMsg frames
//! (snapshot, deltas, tool events, approvals) flow back out. The tunnel
//! lives until the server stops or the socket closes.
//!
//! Trust model: possession of the share key is the credential (like
//! Claude Code share links). Tunnel frames are trusted because the key
//! was minted by this server.

use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::dispatch::{handle_frame, ConnCtx, Flow, ServerState};
use modron_core::SessionId;

/// Build the relay URLs for a bridge base + share key: the agent tunnel
/// `ws(s)://host/agent/{key}` and the viewer page `base/s/{key}`.
pub fn tunnel_urls(bridge_base: &str, key: &str) -> (String, String) {
    let base = bridge_base.trim_end_matches('/');
    let ws_scheme = if base.starts_with("wss://") || base.starts_with("https://") {
        "wss"
    } else {
        "ws"
    };
    let host = base
        .strip_prefix("wss://")
        .or_else(|| base.strip_prefix("ws://"))
        .or_else(|| base.strip_prefix("https://"))
        .or_else(|| base.strip_prefix("http://"))
        .unwrap_or(base);
    (
        format!("{ws_scheme}://{host}/agent/{key}"),
        format!("{base}/s/{key}"),
    )
}

/// Generate a share key: short, lowercase, URL-safe (uuid-derived, no new
/// dependencies).
pub fn share_key() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}

/// Run the tunnel for one session until the socket closes or the server
/// shuts down. Frames from the relay are dispatched exactly like local
/// client frames; outbound frames (snapshot, deltas, events) are relayed
/// to the bridge.
pub async fn run_tunnel(
    state: Arc<ServerState>,
    session: SessionId,
    bridge_base: String,
    key: String,
) -> Result<()> {
    let (agent_url, _open_url) = tunnel_urls(&bridge_base, &key);
    let (socket, _) = connect_async(agent_url.clone())
        .await
        .with_context(|| format!("connecting to the remote-control bridge at {agent_url}"))?;
    let (mut sink, mut stream) = socket.split();

    // Server->relay writer: drains the connection's outbound frames (the
    // dispatch pipeline pushes every reply here via ConnCtx).
    let (out_tx, mut out_rx) = mpsc::unbounded_channel();
    let mut ctx = ConnCtx::new(out_tx);

    // Handshake + eager subscribe before handing the sink to the writer.
    let hello = modron_protocol::Frame::request(
        0,
        modron_protocol::Payload::Hello {
            client: "modron-remote-control".into(),
            protocol: modron_protocol::PROTOCOL_VERSION,
            token: None,
        },
    );
    let attach = modron_protocol::Frame::request(
        1,
        modron_protocol::Payload::Attach {
            session_id: session,
            last_seq: 0,
        },
    );
    for frame in [hello, attach] {
        let text: String = serde_json::to_string(&frame).context("frame serialization")?;
        sink.send(WsMessage::Text(text.into())).await?;
    }

    let writer = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            let text: String = match serde_json::to_string(&frame) {
                Ok(text) => text,
                Err(_) => break,
            };
            if sink.send(WsMessage::Text(text.into())).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    // Relay loop: every frame from the bridge goes through the SAME
    // dispatch as local clients (loopback-trusted: the tunnel key was
    // minted by this server).
    loop {
        let frame = tokio::select! {
            frame = stream.next() => match frame {
                Some(Ok(WsMessage::Text(text))) => serde_json::from_str::<
                    modron_protocol::Frame,
                >(&text)
                .ok(),
                _ => break,
            },
            _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
                Some(modron_protocol::Frame::request(
                    0,
                    modron_protocol::Payload::Ping {},
                ))
            }
        };
        let Some(frame) = frame else {
            continue;
        };
        if handle_frame(&state, &mut ctx, true, frame).await == Flow::Close {
            break;
        }
    }
    drop(ctx);
    let _ = writer.await;
    Ok(())
}

/// Register a remote-control tunnel for `session`: validates the session,
/// mints the share key, spawns the tunnel task. Returns the open URL.
pub fn start_remote_control(
    state: &Arc<ServerState>,
    session: SessionId,
    working_dir: &Path,
    bridge_base: &str,
) -> Result<String> {
    if !bridge_base.starts_with("ws://") && !bridge_base.starts_with("wss://") {
        return Err(anyhow!(
            "bridge url must start with ws:// or wss:// (deploy cloud/bridge first)"
        ));
    }
    if !working_dir.join(".modron").exists() {
        return Err(anyhow!(
            "remote-control hands the agent its working directory - run from inside a \
             project (git repo) rather than an empty dir"
        ));
    }
    state
        .hub
        .handle(&session)
        .ok_or_else(|| anyhow!("no live session {session} - start a task or chat first"))?;
    let key = share_key();
    let (tunnel_url, open_url) = tunnel_urls(bridge_base, &key);
    let state = Arc::clone(state);
    let bridge = bridge_base.to_string();
    tokio::spawn(async move {
        if let Err(e) = run_tunnel(state, session, bridge, key.clone()).await {
            tracing::warn!(key = %key, error = %e, "remote-control tunnel ended");
        }
    });
    let _ = tunnel_url;
    Ok(open_url)
}
