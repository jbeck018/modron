//! rc.rs — remote-control tunnels (the `/remote-control` flow): one
//! outbound WebSocket from this server to a relay bridge (a deployed
//! cloud/bridge worker), keyed by a share code. The relay is
//! transport-only for live traffic: it relays Frames and keeps only a
//! bounded replay buffer per session. Published pages are kept by the
//! relay for a limited time (see cloud/bridge/README.md).
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

use crate::dispatch::Outbound;
use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::dispatch::{handle_frame, ConnCtx, Flow, ServerState};
use modron_core::SessionId;

/// The key that goes into a share link: a one-way hash of the agent's
/// secret key. The bridge derives the same value from the agent's
/// connection, so both sides meet in one relay session, but someone holding
/// a share link cannot recover the agent key and connect as the agent.
/// Must match `viewerKeyOf` in cloud/bridge/src/index.ts.
pub fn viewer_key(agent_key: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("modron-viewer:{agent_key}").as_bytes());
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

/// Build the relay URLs for a bridge base + agent key: the agent tunnel
/// `ws(s)://host/agent/{key}` and the viewer page `base/s/{viewer key}`
/// (see [`viewer_key`]; the share link never contains the agent key).
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
        format!("{base}/s/{}", viewer_key(key)),
    )
}

/// Generate a share key: the full 32 lowercase hex chars of a v4 UUID
/// (122 random bits, D4). Uuid-derived, so no new dependencies.
pub fn share_key() -> String {
    uuid::Uuid::new_v4().simple().to_string()
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
    // The agent URL carries the share key: never put it in an error
    // message or a log line, only the bridge base.
    let (agent_url, _open_url) = tunnel_urls(&bridge_base, &key);
    let (socket, _) = connect_async(agent_url)
        .await
        .with_context(|| format!("connecting to the remote-control bridge at {bridge_base}"))?;
    let (mut sink, mut stream) = socket.split();

    // Server->relay writer: drains the connection's outbound frames (the
    // dispatch pipeline pushes every reply here via ConnCtx).
    let (out_tx, mut out_rx) = mpsc::unbounded_channel();
    // The tunnel is authenticated by construction (its key was minted by
    // this server) and pinned to its one session (D5): the relay's frames
    // get the scoped allowlist, never the server-wide one.
    let mut ctx = ConnCtx::scoped(out_tx, session);

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
        while let Some(outbound) = out_rx.recv().await {
            let text = match outbound {
                Outbound::Reply(frame) => serde_json::to_string(&frame),
                // Broadcasts arrive pre-serialized (one JSON build shared
                // by every viewer).
                Outbound::Live(live) => Ok(live.json.to_string()),
            };
            let Ok(text) = text else { break };
            if sink.send(WsMessage::Text(text.into())).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    // Relay loop: every frame from the bridge goes through the SAME
    // dispatch as local clients, under the tunnel's scope (see ConnCtx::scoped).
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

/// The share key minted for a session by /remote-control, if any.
pub fn share_key_for(state: &ServerState, session: &SessionId) -> Option<String> {
    state.rc_keys.lock().unwrap().get(session).cloned()
}

/// Environment variable holding the bridge's `PUBLISH_TOKEN` secret.
pub const PUBLISH_TOKEN_ENV: &str = "MODRON_PUBLISH_TOKEN";

/// Read the publish token through `lookup` (the process environment in
/// production, a closure in tests). Empty counts as unset.
fn publish_token_from(lookup: impl Fn(&str) -> Option<String>) -> Result<String> {
    lookup(PUBLISH_TOKEN_ENV)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| {
            anyhow!("publishing needs MODRON_PUBLISH_TOKEN (the bridge's PUBLISH_TOKEN secret)")
        })
}

/// The `Authorization` header value the bridge expects for publishing.
pub fn publish_auth_header(token: &str) -> String {
    format!("Bearer {token}")
}

/// Publish an assistant answer as a shareable web page on the bridge.
/// The request is authorized by the deploy-time `PUBLISH_TOKEN` (read from
/// `MODRON_PUBLISH_TOKEN`), never by the share key. `key` only selects the
/// bridge's storage object. Pages are served at /p/<id> with no auth.
/// Fails before sending anything if the token is unset or empty.
pub async fn publish_page(
    bridge_base: &str,
    key: &str,
    session: SessionId,
    title: &str,
    content_md: &str,
) -> Result<String> {
    publish_page_with_lookup(
        |name| std::env::var(name).ok(),
        bridge_base,
        key,
        session,
        title,
        content_md,
    )
    .await
}

/// `publish_page` with the environment lookup injected (tests use this so
/// they never mutate process env).
async fn publish_page_with_lookup(
    lookup: impl Fn(&str) -> Option<String>,
    bridge_base: &str,
    key: &str,
    session: SessionId,
    title: &str,
    content_md: &str,
) -> Result<String> {
    let token = publish_token_from(lookup)?;
    let base = bridge_base.trim_end_matches('/');
    let host = base
        .strip_prefix("wss://")
        .or_else(|| base.strip_prefix("ws://"))
        .or_else(|| base.strip_prefix("https://"))
        .or_else(|| base.strip_prefix("http://"))
        .ok_or_else(|| anyhow!("bridge url must start with ws:// or wss://"))?;
    let http = if base.starts_with("wss://") || base.starts_with("https://") {
        "https"
    } else {
        "http"
    };
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "title": title,
        "session": session.to_string(),
        "content_md": content_md,
    });
    let response = client
        .post(format!("{http}://{host}/pages/{key}"))
        .header("content-type", "application/json")
        .header("authorization", publish_auth_header(&token))
        .body(body.to_string())
        .send()
        .await
        .context("posting the page to the bridge")?;
    if !response.status().is_success() {
        return Err(anyhow!(
            "bridge refused the page: {} {}",
            response.status(),
            response.text().await.unwrap_or_default()
        ));
    }
    let value: serde_json::Value = response.json().await.context("bridge page reply")?;
    let id = value
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("bridge page reply missing id"))?;
    Ok(format!("{http}://{host}/p/{id}"))
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
    state.rc_keys.lock().unwrap().insert(session, key.clone());
    let state_moved = Arc::clone(state);
    let bridge = bridge_base.to_string();
    let tunnel_task = tokio::spawn(async move {
        if let Err(e) = run_tunnel(state_moved, session, bridge, key).await {
            tracing::warn!(%session, error = %e, "remote-control tunnel ended");
        }
    });
    // Abort the previous tunnel for this session, if any: a re-registration
    // used to stack duplicate pumpers on stale bridge sockets forever
    // (cycle-2 audit L7).
    if let Some(old) = state
        .rc_tunnels
        .lock()
        .unwrap()
        .insert(session, tunnel_task.abort_handle())
    {
        old.abort();
    }
    let _ = tunnel_url;
    Ok(open_url)
}

#[cfg(test)]
mod publish_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// Serve one HTTP request on a local port. Returns the base URL and a
    /// receiver that yields the request head (lowercased) once it arrives.
    fn one_shot_bridge() -> (String, std::sync::mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            let head_end = loop {
                let n = sock.read(&mut chunk).unwrap();
                if n == 0 {
                    break buf.len();
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos + 4;
                }
            };
            let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
            let length: usize = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().ok())
                        .flatten()
                })
                .unwrap_or(0);
            while buf.len() < head_end + length {
                let n = sock.read(&mut chunk).unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let body = r#"{"id":"page-1"}"#;
            let reply = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(reply.as_bytes()).unwrap();
            let _ = tx.send(head.to_lowercase());
        });
        (format!("http://{addr}"), rx)
    }

    #[test]
    fn auth_header_is_bearer_token() {
        assert_eq!(publish_auth_header("s3cret"), "Bearer s3cret");
    }

    #[test]
    fn unset_or_empty_token_is_an_error() {
        assert_eq!(
            publish_token_from(|_| Some("t".into())).unwrap(),
            "t".to_string()
        );
        assert!(publish_token_from(|_| Some(String::new())).is_err());
        let message = publish_token_from(|_| None).unwrap_err().to_string();
        assert_eq!(
            message,
            "publishing needs MODRON_PUBLISH_TOKEN (the bridge's PUBLISH_TOKEN secret)"
        );
    }

    #[tokio::test]
    async fn publish_without_token_makes_no_request() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let err = publish_page_with_lookup(
            |_| None,
            &base,
            "0123456789abcdef0123456789abcdef",
            uuid::Uuid::new_v4(),
            "title",
            "body",
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("MODRON_PUBLISH_TOKEN"));
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "no connection may reach the bridge without a token"
        );
    }

    #[tokio::test]
    async fn publish_header_carries_publish_token_not_share_key() {
        let (base, head_rx) = one_shot_bridge();
        let key = "0123456789abcdef0123456789abcdef";
        let url = publish_page_with_lookup(
            |name| (name == PUBLISH_TOKEN_ENV).then(|| "pub-token-xyz".to_string()),
            &base,
            key,
            uuid::Uuid::new_v4(),
            "title",
            "# body",
        )
        .await
        .unwrap();
        assert!(url.ends_with("/p/page-1"), "unexpected url {url}");
        let head = head_rx.recv().unwrap();
        assert!(head.starts_with(&format!("post /pages/{key} ")), "{head}");
        assert!(
            head.contains("authorization: bearer pub-token-xyz"),
            "{head}"
        );
        assert!(
            !head.contains(&format!("bearer {key}")),
            "share key used as bearer: {head}"
        );
    }
}

#[cfg(test)]
mod key_tests {
    use super::{tunnel_urls, viewer_key};

    /// The same vector is produced by `viewerKeyOf` in the bridge
    /// (sha256("modron-viewer:" + key), first 16 bytes, hex). If this
    /// changes, share links stop meeting their agent on the relay.
    #[test]
    fn viewer_key_matches_the_bridge_derivation() {
        assert_eq!(
            viewer_key("0123456789abcdef0123456789abcdef"),
            "1e14cd6450e5f6e627a9c619807f506c"
        );
    }

    #[test]
    fn share_link_never_contains_the_agent_key() {
        let key = "0123456789abcdef0123456789abcdef";
        let (agent_url, share_url) = tunnel_urls("wss://bridge.example", key);
        assert_eq!(agent_url, format!("wss://bridge.example/agent/{key}"));
        assert!(!share_url.contains(key), "{share_url}");
        assert_eq!(
            share_url,
            format!("wss://bridge.example/s/{}", viewer_key(key))
        );
    }
}
