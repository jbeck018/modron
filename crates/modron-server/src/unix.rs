//! The local Unix-socket listener (Phase 2): `~/.modron/serve.sock`,
//! newline-delimited JSON [`Frame`]s — the same wire vocabulary as the
//! WebSocket transport, framed like the existing daemon (daemon.rs).
//! Unix-socket peers are always trusted: the file's permissions are the
//! authentication.

use crate::dispatch::Outbound;
use std::sync::Arc;

use modron_protocol::Frame;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

use crate::dispatch::{handle_frame, send, ConnCtx, Flow, ServerState};

/// Accept loop for the bound Unix socket.
pub async fn run_unix_listener(listener: UnixListener, state: Arc<ServerState>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(e) = serve_unix_connection(stream, state).await {
                tracing::debug!(error = %e, "unix-socket connection ended");
            }
        });
    }
}

/// Unix sockets are trusted peers: no device token required.
async fn serve_unix_connection(stream: UnixStream, state: Arc<ServerState>) -> anyhow::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Outbound>();
    let writer_task = tokio::spawn(async move {
        while let Some(outbound) = out_rx.recv().await {
            let mut line = match outbound {
                Outbound::Reply(frame) => {
                    let Ok(line) = serde_json::to_string(&frame) else {
                        break;
                    };
                    line
                }
                // Broadcasts arrive pre-serialized (one JSON build shared
                // by every viewer).
                Outbound::Live(live) => live.json.to_string(),
            };
            line.push('\n');
            if writer.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });

    let mut ctx = ConnCtx::new(out_tx.clone());
    let mut flow = Flow::Continue;
    while flow == Flow::Continue {
        // One invalid-UTF-8 line must not kill a trusted local connection
        // (cycle-2 audit L9): skip + warn, same posture as the TUI's pump.
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                tracing::warn!("unix socket: skipping non-UTF-8 line");
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        if line.len() > 16 * 1024 * 1024 {
            tracing::warn!(bytes = line.len(), "unix socket: oversized line skipped");
            continue;
        };
        match serde_json::from_str::<Frame>(&line) {
            Ok(frame) => flow = handle_frame(&state, &mut ctx, true, frame).await,
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

    drop(ctx);
    drop(out_tx);
    let _ = writer_task.await;
    Ok(())
}
