//! The local Unix-socket listener (Phase 2): `~/.modron/serve.sock`,
//! newline-delimited JSON [`Frame`]s — the same wire vocabulary as the
//! WebSocket transport, framed like the existing daemon (daemon.rs).
//! Unix-socket peers are always trusted: the file's permissions are the
//! authentication.

use crate::dispatch::Outbound;
use std::sync::Arc;

use modron_protocol::Frame;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
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

/// Largest accepted line (the same 16 MiB cap as before; now enforced
/// while reading, so an oversized line never fully allocates).
const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

/// One line read by [`read_line_capped`].
#[derive(Debug, PartialEq, Eq)]
enum LineRead {
    /// A complete line, newline and any trailing `\r` removed.
    Line(Vec<u8>),
    /// The line exceeded the cap; its bytes were discarded up to the newline.
    Oversized,
}

/// Read one newline-delimited line, holding at most `max` bytes. Returns
/// `None` at end of stream. A line without a trailing newline at EOF is
/// returned as-is, like `BufRead::lines`.
async fn read_line_capped<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max: usize,
) -> std::io::Result<Option<LineRead>> {
    let mut line = Vec::new();
    let mut oversized = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(match (oversized, line.is_empty()) {
                (true, _) => Some(LineRead::Oversized),
                (false, true) => None,
                (false, false) => Some(LineRead::Line(line)),
            });
        }
        let newline_at = available.iter().position(|b| *b == b'\n');
        let newline = newline_at.is_some();
        let end = newline_at.unwrap_or(available.len());
        let used = if newline { end + 1 } else { end };
        // A CRLF terminator's '\r' is not part of the line, so it does not
        // count against the cap.
        let mut segment = &available[..end];
        if newline {
            segment = segment.strip_suffix(b"\r").unwrap_or(segment);
        }
        if !oversized {
            if line.len() + segment.len() > max {
                oversized = true;
                line = Vec::new();
            } else {
                line.extend_from_slice(segment);
            }
        }
        reader.consume(used);
        if newline {
            if oversized {
                return Ok(Some(LineRead::Oversized));
            }
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(Some(LineRead::Line(line)));
        }
    }
}

/// Unix sockets are trusted peers: no device token required.
async fn serve_unix_connection(stream: UnixStream, state: Arc<ServerState>) -> anyhow::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

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
        let line = match read_line_capped(&mut reader, MAX_LINE_BYTES).await? {
            Some(LineRead::Line(bytes)) => bytes,
            Some(LineRead::Oversized) => {
                tracing::warn!("unix socket: oversized line skipped");
                continue;
            }
            None => break,
        };
        // One invalid-UTF-8 line must not kill a trusted local connection
        // (cycle-2 audit L9): skip + warn, same posture as the TUI's pump.
        let Ok(line) = String::from_utf8(line) else {
            tracing::warn!("unix socket: skipping non-UTF-8 line");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn over_long_line_is_rejected_and_stream_resyncs() {
        let input = b"aaaaaaaaaaaaaaaa\nok\n".to_vec();
        let mut reader = BufReader::new(&input[..]);
        assert_eq!(
            read_line_capped(&mut reader, 8).await.unwrap(),
            Some(LineRead::Oversized)
        );
        assert_eq!(
            read_line_capped(&mut reader, 8).await.unwrap(),
            Some(LineRead::Line(b"ok".to_vec()))
        );
        assert_eq!(read_line_capped(&mut reader, 8).await.unwrap(), None);
    }

    #[tokio::test]
    async fn line_at_cap_is_kept_and_crlf_trimmed() {
        let input = b"abcd\r\n".to_vec();
        let mut reader = BufReader::new(&input[..]);
        assert_eq!(
            read_line_capped(&mut reader, 4).await.unwrap(),
            Some(LineRead::Line(b"abcd".to_vec()))
        );
    }

    #[tokio::test]
    async fn unterminated_final_line_is_returned() {
        let input = b"tail".to_vec();
        let mut reader = BufReader::new(&input[..]);
        assert_eq!(
            read_line_capped(&mut reader, 16).await.unwrap(),
            Some(LineRead::Line(b"tail".to_vec()))
        );
    }
}
