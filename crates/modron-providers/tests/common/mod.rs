//! Scripted one-shot SSE server shared by the S1 streaming fixture tests.
//!
//! Raw `TcpListener` + hand-written HTTP/1.1: the request is read in full
//! (headers + Content-Length body) and recorded, then the scripted response
//! segments are written and the socket is closed — a close-delimited body,
//! no chunked encoding, exactly the SSE shape real endpoints emit.

use serde_json::Value;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// What the client actually sent, for wire-shape assertions.
#[derive(Debug)]
pub struct RecordedRequest {
    pub path: String,
    pub body: Value,
    pub x_api_key: Option<String>,
    pub anthropic_version: Option<String>,
    pub authorization: Option<String>,
}

/// Bind a one-shot SSE server on 127.0.0.1:0. The returned task accepts
/// exactly one connection, reads the whole request, writes `segments` (each
/// after its delay in ms) and closes the socket.
pub async fn spawn_sse_server(
    segments: Vec<(Vec<u8>, u64)>,
) -> (
    std::net::SocketAddr,
    tokio::task::JoinHandle<RecordedRequest>,
) {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind test server");
    let addr = listener.local_addr().expect("local addr");
    let handle = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept one connection");

        // Read until end of headers.
        let mut raw: Vec<u8> = Vec::new();
        let mut scratch = [0u8; 4096];
        let header_end = loop {
            if let Some(pos) = find_subsequence(&raw, b"\r\n\r\n") {
                break pos + 4;
            }
            let n = socket.read(&mut scratch).await.expect("read request head");
            assert!(n > 0, "client closed before sending headers");
            raw.extend_from_slice(&scratch[..n]);
        };
        // Then read the Content-Length-delimited body.
        let head = String::from_utf8_lossy(&raw[..header_end]).into_owned();
        let body_len = content_length(&head).unwrap_or(0);
        while raw.len() < header_end + body_len {
            let n = socket.read(&mut scratch).await.expect("read request body");
            assert!(n > 0, "client closed before sending body");
            raw.extend_from_slice(&scratch[..n]);
        }

        let mut lines = head.split("\r\n");
        let request_line = lines.next().unwrap_or_default();
        let mut parts = request_line.split_whitespace();
        let _method = parts.next().unwrap_or_default();
        let path = parts.next().unwrap_or_default().to_string();
        let mut x_api_key = None;
        let mut anthropic_version = None;
        let mut authorization = None;
        for line in lines {
            if let Some((name, value)) = line.split_once(':') {
                match name.trim().to_ascii_lowercase().as_str() {
                    "x-api-key" => x_api_key = Some(value.trim().to_string()),
                    "anthropic-version" => anthropic_version = Some(value.trim().to_string()),
                    "authorization" => authorization = Some(value.trim().to_string()),
                    _ => {}
                }
            }
        }
        let body: Value = serde_json::from_slice(&raw[header_end..header_end + body_len])
            .expect("request body is JSON");

        for (segment, delay_ms) in segments {
            if delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
            socket
                .write_all(&segment)
                .await
                .expect("write response segment");
            socket.flush().await.expect("flush response segment");
        }
        // Close-delimited body: EOF ends it.
        let _ = socket.shutdown().await;
        RecordedRequest {
            path,
            body,
            x_api_key,
            anthropic_version,
            authorization,
        }
    });
    (addr, handle)
}

pub fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

pub fn content_length(head: &str) -> Option<usize> {
    for line in head.split("\r\n").skip(1) {
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                return value.trim().parse().ok();
            }
        }
    }
    None
}

/// An HTTP/1.1 response whose body ends at connection close (no
/// Content-Length, no chunked encoding) — the SSE shape this module proves.
pub fn sse_response(body: &[u8]) -> Vec<u8> {
    let mut response = b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: close\r\n\r\n"
        .to_vec();
    response.extend_from_slice(body);
    response
}

/// Assemble an SSE body from lines. Blank lines between events are empty
/// string elements; the terminating newline of the last data line plus the
/// blank line that dispatches the final event are appended here.
pub fn sse_body(lines: &[&str]) -> Vec<u8> {
    let mut body = lines.join("\n");
    body.push_str("\n\n");
    body.into_bytes()
}

/// Split `body` so a TCP segment boundary lands inside `marker` (mid-JSON),
/// with a small delay to encourage separate reads.
pub fn split_at_marker(body: &[u8], marker: &str) -> Vec<(Vec<u8>, u64)> {
    let found = body
        .windows(marker.len())
        .position(|window| window == marker.as_bytes())
        .expect("marker present in fixture body");
    let cut = found + marker.len() / 2;
    vec![(body[..cut].to_vec(), 5), (body[cut..].to_vec(), 5)]
}
