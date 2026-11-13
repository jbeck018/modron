//! Shared HTTP scaffolding for the provider crates: one `build_client`
//! (the three providers shipped byte-identical copies), the POST helpers
//! wrapping the status-check -> error -> parse sequence, and the 401
//! refresh-and-retry wrapper shared by the subscription providers
//! (2026-11-12 cycle-2 audit DRY fix: the helpers the old doc claimed but
//! never shipped).
//!
//! Every error string produced here is BYTE-IDENTICAL to the inline code
//! it replaced — [`crate::retry::is_retryable`] classifies retries by
//! message prefixes (`timeout: …`, `connection error: …`,
//! `request failed: …`, `HTTP <status>: <snippet>`), so a wording change
//! would silently break the retry policy.

use modron_core::{Error, Result};
use reqwest::Client;
use std::time::Duration;

/// Per-request timeout shared by every provider.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// Connect timeout shared by every provider.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The shared reqwest client: same timeouts, same construction everywhere.
pub fn build_client() -> Result<Client> {
    Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .map_err(|err| Error::Other(anyhow::anyhow!("failed to build HTTP client: {err}")))
}

/// Send a POST with fully caller-supplied headers and JSON body. Headers
/// are the caller's problem by design — anthropic authenticates with
/// `x-api-key` (+ `anthropic-version`), the OpenAI shapes with a Bearer —
/// so this owns only transport-error mapping.
async fn post(
    client: &Client,
    provider_id: &str,
    url: &str,
    headers: &[(&str, String)],
    body: &serde_json::Value,
) -> Result<reqwest::Response> {
    let mut request = client.post(url);
    for (name, value) in headers {
        request = request.header(*name, value);
    }
    request
        .json(body)
        .send()
        .await
        .map_err(|err| crate::transport_error(provider_id, err))
}

/// Consume a response's error body into the canonical
/// `HTTP <status>: <snippet>` error when the status is not a success;
/// otherwise pass the response through untouched (the SSE paths keep
/// reading its byte stream). Streaming failures arrive as a plain
/// (non-SSE) error body, which is exactly what gets snippet-embedded.
pub(crate) async fn check_status(
    provider_id: &str,
    response: reqwest::Response,
) -> Result<reqwest::Response> {
    let status = response.status();
    if !status.is_success() {
        let text = response
            .text()
            .await
            .map_err(|err| crate::transport_error(provider_id, err))?;
        return Err(crate::http_error(provider_id, status, &text));
    }
    Ok(response)
}

/// Text + status-check + parse tail for a response already in hand (the
/// anthropic path wraps its POST in the 401 retry first, so it cannot go
/// through [`post_json`] wholesale).
pub(crate) async fn finish_json(
    provider_id: &str,
    response: reqwest::Response,
) -> Result<serde_json::Value> {
    let response = check_status(provider_id, response).await?;
    let text = response
        .text()
        .await
        .map_err(|err| crate::transport_error(provider_id, err))?;
    serde_json::from_str(&text).map_err(|err| Error::Provider {
        provider: provider_id.to_string(),
        message: format!("malformed response: {err}"),
    })
}

/// POST a JSON body and parse the JSON answer: the exact sequence the
/// non-streaming turn paths share — transport-error mapping, the canonical
/// `HTTP <status>: <snippet>` failure, then `malformed response: …`.
/// Headers are fully caller-supplied (see [`post`]); the helper owns only
/// POST + status->error + parse (2026-11-12 audit DRY fix).
pub async fn post_json(
    client: &Client,
    provider_id: &str,
    url: &str,
    headers: &[(&str, String)],
    body: &serde_json::Value,
) -> Result<serde_json::Value> {
    let response = post(client, provider_id, url, headers, body).await?;
    finish_json(provider_id, response).await
}

/// One 401-refresh-retry cycle for the subscription providers: send, and
/// on a 401 try `refresh`; when the credentials were rotated, resend. A
/// failed refresh does not mask the original 401 — the server's
/// diagnostic is what the caller needs to see (revoked token, expired
/// grant). anthropic and openai-responses shipped this wrapper verbatim
/// twice; it is one function now (2026-11-12 audit DRY fix).
pub async fn with_401_retry<SendFut, SendFn, RefreshFut, RefreshFn>(
    mut send: SendFn,
    mut refresh: RefreshFn,
) -> Result<reqwest::Response>
where
    SendFn: FnMut() -> SendFut,
    SendFut: std::future::Future<Output = Result<reqwest::Response>>,
    RefreshFn: FnMut() -> RefreshFut,
    RefreshFut: std::future::Future<Output = Result<bool>>,
{
    let mut response = send().await?;
    if response.status().as_u16() == 401 {
        match refresh().await {
            Ok(true) => response = send().await?,
            Ok(false) => {}
            Err(refresh_err) => {
                tracing::warn!("{refresh_err:#} — surfacing the original 401");
            }
        }
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Serve `responses` (raw HTTP/1.1 bytes, one per connection) on a
    /// one-shot loopback listener. Each connection reads one request
    /// buffer, replies, and closes.
    async fn serve_once(responses: Vec<String>) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            for response in responses {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let mut scratch = [0u8; 4096];
                // Drain at least the request head; the reply follows.
                let _ = socket.read(&mut scratch).await;
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        addr
    }

    fn response(status_line: &str, body: &str) -> String {
        format!(
            "{status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// The 401 wrapper: one rotation + resend on the first 401, no retry
    /// when refresh reports no rotation, and the original 401 surfaces
    /// when the refresh fails.
    #[tokio::test]
    async fn retries_once_after_a_refresh() {
        let client = build_client().unwrap();
        let addr = serve_once(vec![
            response("HTTP/1.1 401 Unauthorized", r#"{"error":"expired"}"#),
            response("HTTP/1.1 200 OK", r#"{"ok":true}"#),
        ])
        .await;
        let url = format!("http://{addr}/");
        let mut refreshes = 0;
        let response = with_401_retry(
            || {
                let url = url.clone();
                let client = client.clone();
                async move {
                    client
                        .post(&url)
                        .send()
                        .await
                        .map_err(|err| crate::transport_error("test", err))
                }
            },
            || {
                refreshes += 1;
                std::future::ready(Ok(true))
            },
        )
        .await
        .expect("retried after refresh");
        assert_eq!(refreshes, 1, "one refresh");
        let value = finish_json("test", response).await.expect("json body");
        assert_eq!(value["ok"], true);
    }

    #[tokio::test]
    async fn no_rotation_surfaces_the_401() {
        let client = build_client().unwrap();
        let addr = serve_once(vec![response(
            "HTTP/1.1 401 Unauthorized",
            r#"{"error":"revoked"}"#,
        )])
        .await;
        let url = format!("http://{addr}/");
        let response = with_401_retry(
            || {
                let url = url.clone();
                let client = client.clone();
                async move {
                    client
                        .post(&url)
                        .send()
                        .await
                        .map_err(|err| crate::transport_error("test", err))
                }
            },
            || std::future::ready(Ok(false)),
        )
        .await
        .expect("no retry, but still a response");
        // The 401 passes through for the CALLER to map (check_status /
        // finish_json produce the canonical `HTTP 401 …` text downstream).
        assert_eq!(response.status().as_u16(), 401);
    }

    #[tokio::test]
    async fn failed_refresh_does_not_mask_the_401() {
        let client = build_client().unwrap();
        let addr = serve_once(vec![response(
            "HTTP/1.1 401 Unauthorized",
            r#"{"error":"revoked"}"#,
        )])
        .await;
        let url = format!("http://{addr}/");
        let response = with_401_retry(
            || {
                let url = url.clone();
                let client = client.clone();
                async move {
                    client
                        .post(&url)
                        .send()
                        .await
                        .map_err(|err| crate::transport_error("test", err))
                }
            },
            || {
                std::future::ready(Err(Error::Provider {
                    provider: "test".into(),
                    message: "refresh blew up".into(),
                }))
            },
        )
        .await
        .expect("a failed refresh never masks the 401: the original response wins");
        assert_eq!(
            response.status().as_u16(),
            401,
            "the original 401 is what the caller maps"
        );
    }

    /// post_json's error sequence is byte-identical to the inline code it
    /// replaced: the status body snippet + the malformed-response shape.
    #[tokio::test]
    async fn post_json_maps_status_and_parse_errors() {
        let client = build_client().unwrap();
        let addr = serve_once(vec![response(
            "HTTP/1.1 429 Too Many Requests",
            "slow down\nsecond line",
        )])
        .await;
        let url = format!("http://{addr}/");
        let headers = vec![("Authorization", "Bearer sk-test".to_string())];
        let err = post_json(&client, "test", &url, &headers, &serde_json::json!({}))
            .await
            .expect_err("429 is an error");
        assert_eq!(
            err.to_string(),
            "provider `test` error: HTTP 429 Too Many Requests: slow down second line",
            "snippet flattens newlines and truncates like the inline path"
        );

        // A success whose body is not JSON is a malformed-response error.
        let addr = serve_once(vec![response("HTTP/1.1 200 OK", "not json")]).await;
        let url = format!("http://{addr}/");
        let err = post_json(&client, "test", &url, &headers, &serde_json::json!({}))
            .await
            .expect_err("garbage body");
        assert!(err.to_string().contains("malformed response:"), "{err}");
    }
}
