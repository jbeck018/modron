//! Streamable HTTP transport, client side (MCP spec 2025-03-26 and later).
//!
//! Each JSON-RPC message is one `POST` to the server URL with
//! `Accept: application/json, text/event-stream`. The reply is either a single
//! `application/json` body or a `text/event-stream` (SSE) body that is read
//! until the response with the awaited id arrives; other events are skipped.
//! A 202 with no body is the normal reply to notifications.
//!
//! Sessions: when the initialize reply carries `Mcp-Session-Id`, that value is
//! sent on every later request, and `MCP-Protocol-Version` carries the
//! negotiated version. A 404 on a request that carried a session id means the
//! session expired: the client re-initializes once and retries that request
//! once.
//!
//! Headers come from config with `${ENV_VAR}` templates expanded at connect
//! time; an unset variable is a connect error that names the variable, never
//! its value. Header values are never logged.
//!
//! Out of scope: the legacy HTTP+SSE transport (GET endpoint discovery), OAuth
//! flows, and server-initiated requests (sampling, roots, elicitation). Server
//! messages other than the awaited response are skipped.

use crate::MAX_LINE_BYTES;
use anyhow::anyhow;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE};
use reqwest::StatusCode;
use serde_json::{json, Value};
use std::time::Duration;

/// Response bodies (JSON or SSE) are capped at the same size as stdio lines.
const MAX_BODY_BYTES: usize = MAX_LINE_BYTES;
/// Characters of an error body echoed into a non-2xx error.
const ERROR_SNIPPET_CHARS: usize = 200;

const MCP_SESSION_ID: &str = "mcp-session-id";
const MCP_PROTOCOL_VERSION: &str = "mcp-protocol-version";

/// Client side of one Streamable HTTP MCP server.
pub(crate) struct HttpLink {
    client: reqwest::Client,
    url: reqwest::Url,
    /// Configured headers (already expanded) plus the fixed Accept and
    /// Content-Type. Session and protocol headers are added per request.
    headers: HeaderMap,
    /// Params of the initialize request, kept for the re-initialize path.
    init_params: Value,
    session: Option<HeaderValue>,
    protocol_version: Option<HeaderValue>,
    next_id: u64,
}

impl HttpLink {
    pub(crate) fn new(
        server_name: &str,
        url: &str,
        headers: &[(String, String)],
        init_params: Value,
    ) -> anyhow::Result<Self> {
        // The url itself may carry secrets in its query, so it never appears
        // in an error message.
        let url = reqwest::Url::parse(url)
            .map_err(|_| anyhow!("MCP server `{server_name}` has an invalid url"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(anyhow!(
                "MCP server `{server_name}` url must use http or https"
            ));
        }
        let mut map = HeaderMap::new();
        for (name, template) in headers {
            let value = expand_env_templates(name, template, |var| std::env::var(var).ok())
                .map_err(|e| anyhow!("MCP server `{server_name}`: {e}"))?;
            let header_name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| anyhow!("MCP server `{server_name}`: invalid header name `{name}`"))?;
            let header_value = HeaderValue::from_str(&value).map_err(|_| {
                anyhow!("MCP server `{server_name}`: header `{name}` has an invalid value")
            })?;
            map.insert(header_name, header_value);
        }
        map.insert(
            ACCEPT,
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        map.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        // Loopback never needs a proxy; a proxy env var would otherwise
        // swallow requests to a local MCP server.
        let mut builder = reqwest::Client::builder();
        if matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")) {
            builder = builder.no_proxy();
        }
        let client = builder
            .build()
            .map_err(|e| anyhow!("MCP server `{server_name}`: HTTP client failed: {e}"))?;
        Ok(Self {
            client,
            url,
            headers: map,
            init_params,
            session: None,
            protocol_version: None,
            next_id: 0,
        })
    }

    /// One request/response exchange. A 404 on a request that carried a
    /// session id re-initializes once and retries once.
    pub(crate) async fn request(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> anyhow::Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Some(result) = self.send(&message, Some(id), method, timeout).await? {
            return Ok(result);
        }
        tracing::info!(method, "MCP HTTP session expired; re-initializing once");
        self.reinitialize(timeout).await?;
        self.next_id += 1;
        let id = self.next_id;
        let retry = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.send(&retry, Some(id), method, timeout)
            .await?
            .ok_or_else(|| {
                anyhow!("MCP session expired again right after re-initialization (HTTP 404)")
            })
    }

    /// Fire-and-forget notification; failures are logged, as on stdio.
    pub(crate) async fn notify(&mut self, method: &str, params: Value, timeout: Duration) {
        let message = json!({"jsonrpc": "2.0", "method": method, "params": params});
        if let Err(e) = self.send(&message, None, method, timeout).await {
            tracing::warn!(error = %e, "failed to send MCP notification");
        }
    }

    /// Record the protocol version the server negotiated. Falls back to the
    /// version we asked for when the reply omits it.
    pub(crate) fn adopt_protocol_version(&mut self, initialize_result: &Value) {
        let requested = self
            .init_params
            .get("protocolVersion")
            .and_then(|v| v.as_str());
        let negotiated = initialize_result
            .get("protocolVersion")
            .and_then(|v| v.as_str())
            .or(requested);
        self.protocol_version = negotiated.and_then(|v| HeaderValue::from_str(v).ok());
    }

    async fn reinitialize(&mut self, timeout: Duration) -> anyhow::Result<()> {
        self.session = None;
        self.next_id += 1;
        let id = self.next_id;
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": self.init_params.clone(),
        });
        let result = self
            .send(&message, Some(id), "initialize", timeout)
            .await?
            .ok_or_else(|| anyhow!("MCP re-initialize received no reply"))?;
        self.adopt_protocol_version(&result);
        self.notify("notifications/initialized", json!({}), timeout)
            .await;
        Ok(())
    }

    /// POST one message. Returns `Ok(None)` only for a 404 on a request that
    /// carried a session id (the session expired). For a notification
    /// (`id == None`) returns `Ok(Some(Null))` on success.
    async fn send(
        &mut self,
        message: &Value,
        id: Option<u64>,
        method: &str,
        timeout: Duration,
    ) -> anyhow::Result<Option<Value>> {
        let body = serde_json::to_vec(message)?;
        let mut request = self
            .client
            .post(self.url.clone())
            .headers(self.headers.clone())
            .timeout(timeout)
            .body(body);
        let sent_session = self.session.is_some();
        if let Some(session) = &self.session {
            request = request.header(MCP_SESSION_ID, session.clone());
        }
        if let Some(version) = &self.protocol_version {
            request = request.header(MCP_PROTOCOL_VERSION, version.clone());
        }
        let mut response = request
            .send()
            .await
            .map_err(|e| transport_error(method, e))?;
        if let Some(session) = response.headers().get(MCP_SESSION_ID) {
            self.session = Some(session.clone());
        }
        let status = response.status();
        if status == StatusCode::NOT_FOUND && sent_session {
            return Ok(None);
        }
        if !status.is_success() {
            let snippet = match read_body(&mut response, MAX_BODY_BYTES, method).await {
                Ok(bytes) => String::from_utf8_lossy(&bytes)
                    .chars()
                    .take(ERROR_SNIPPET_CHARS)
                    .collect::<String>(),
                Err(_) => String::new(),
            };
            return Err(if snippet.is_empty() {
                anyhow!("MCP server returned HTTP {status} for `{method}`")
            } else {
                anyhow!("MCP server returned HTTP {status} for `{method}`: {snippet}")
            });
        }
        let Some(id) = id else {
            return Ok(Some(Value::Null));
        };

        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let envelope = if content_type.starts_with("text/event-stream") {
            read_sse_reply(&mut response, id, method).await?
        } else if content_type.starts_with("application/json") {
            let bytes = read_body(&mut response, MAX_BODY_BYTES, method).await?;
            let value: Value = serde_json::from_slice(&bytes)
                .map_err(|e| anyhow!("MCP `{method}` reply is not valid JSON: {e}"))?;
            find_reply(&value, id)
                .ok_or_else(|| anyhow!("MCP `{method}` reply carried no response for id {id}"))?
        } else {
            return Err(anyhow!(
                "MCP server answered `{method}` with HTTP {status} and no JSON or event-stream body"
            ));
        };
        into_result(&envelope).map(Some)
    }
}

/// Map a reqwest failure to a message without its URL.
fn transport_error(method: &str, error: reqwest::Error) -> anyhow::Error {
    if error.is_timeout() {
        return anyhow!("timed out waiting for `{method}` response");
    }
    let error = error.without_url();
    let mut message = error.to_string();
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    anyhow!("MCP HTTP request for `{method}` failed: {message}")
}

/// Read a whole body, refusing anything past `cap` bytes (checked against
/// Content-Length first, then against the bytes actually received).
async fn read_body(
    response: &mut reqwest::Response,
    cap: usize,
    method: &str,
) -> anyhow::Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|len| len > cap as u64)
    {
        return Err(anyhow!(
            "MCP HTTP response exceeds {cap} bytes — refusing to buffer more"
        ));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| transport_error(method, e))?
    {
        if body.len() + chunk.len() > cap {
            return Err(anyhow!(
                "MCP HTTP response exceeds {cap} bytes — refusing to buffer more"
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Read an SSE reply until the response with `id` arrives.
async fn read_sse_reply(
    response: &mut reqwest::Response,
    id: u64,
    method: &str,
) -> anyhow::Result<Value> {
    let mut parser = SseParser::default();
    let mut total = 0usize;
    loop {
        let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| transport_error(method, e))?
        else {
            return Err(anyhow!(
                "MCP server closed the event stream before answering `{method}`"
            ));
        };
        total += chunk.len();
        if total > MAX_BODY_BYTES {
            return Err(anyhow!(
                "MCP event stream exceeds {MAX_BODY_BYTES} bytes — refusing to buffer more"
            ));
        }
        for event in parser.feed(&chunk) {
            match serde_json::from_str::<Value>(&event) {
                Ok(value) => {
                    if let Some(reply) = find_reply(&value, id) {
                        return Ok(reply);
                    }
                    tracing::debug!("skipping non-matching MCP SSE event");
                }
                Err(e) => tracing::warn!(error = %e, "non-JSON SSE event from MCP server"),
            }
        }
    }
}

/// The JSON-RPC response object with `id`, from a single message or a batch.
fn find_reply(value: &Value, id: u64) -> Option<Value> {
    let matches = |v: &Value| v.get("id").and_then(|i| i.as_u64()) == Some(id);
    match value {
        Value::Array(items) => items.iter().find(|v| matches(v)).cloned(),
        other if matches(other) => Some(other.clone()),
        _ => None,
    }
}

/// Same contract as the stdio path: JSON-RPC error -> Err, else `result`.
fn into_result(envelope: &Value) -> anyhow::Result<Value> {
    if let Some(error) = envelope.get("error") {
        return Err(anyhow!("server error: {error}"));
    }
    envelope
        .get("result")
        .cloned()
        .ok_or_else(|| anyhow!("response missing result: {envelope}"))
}

/// Incremental SSE parser. Handles LF, CRLF and CR line endings, lines split
/// across chunks, comment lines, and multi-line `data:` fields (joined with
/// `\n`). Returns the data payload of each completed event.
#[derive(Default)]
pub(crate) struct SseParser {
    pending: Vec<u8>,
    data: String,
    has_data: bool,
}

impl SseParser {
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        self.pending.extend_from_slice(chunk);
        let mut events = Vec::new();
        let mut start = 0;
        let mut i = 0;
        while i < self.pending.len() {
            let byte = self.pending[i];
            if byte != b'\n' && byte != b'\r' {
                i += 1;
                continue;
            }
            // A trailing CR may be the first half of a CRLF split across
            // chunks: wait for the next byte before deciding.
            if byte == b'\r' && i + 1 == self.pending.len() {
                break;
            }
            let next = if byte == b'\r' && self.pending[i + 1] == b'\n' {
                i + 2
            } else {
                i + 1
            };
            // Line terminators are ASCII, so this never splits a UTF-8 char.
            let line = String::from_utf8_lossy(&self.pending[start..i]).into_owned();
            if let Some(event) = self.line(&line) {
                events.push(event);
            }
            start = next;
            i = next;
        }
        self.pending.drain(..start);
        events
    }

    fn line(&mut self, line: &str) -> Option<String> {
        if line.is_empty() {
            if self.has_data {
                self.has_data = false;
                return Some(std::mem::take(&mut self.data));
            }
            return None;
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        if field == "data" {
            if self.has_data {
                self.data.push('\n');
            }
            self.data.push_str(value);
            self.has_data = true;
        }
        None
    }
}

/// Expand `${NAME}` references in one header value. An unset (or empty)
/// variable is an error naming the variable and the header, never the value.
pub(crate) fn expand_env_templates(
    header: &str,
    template: &str,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<String, String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(pos) = rest.find("${") {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 2..];
        let Some(end) = after.find('}') else {
            return Err(format!("header `{header}` has an unterminated `${{`"));
        };
        let var = &after[..end];
        let well_formed = !var.is_empty()
            && !var.starts_with(|c: char| c.is_ascii_digit())
            && var.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !well_formed {
            return Err(format!(
                "header `{header}` has an invalid variable reference"
            ));
        }
        match lookup(var).filter(|v| !v.is_empty()) {
            Some(value) => out.push_str(&value),
            None => {
                return Err(format!(
                    "header `{header}` references unset environment variable `{var}`"
                ))
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(chunks: &[&[u8]]) -> Vec<String> {
        let mut parser = SseParser::default();
        chunks.iter().flat_map(|c| parser.feed(c)).collect()
    }

    #[test]
    fn sse_joins_multiline_data_and_skips_comments() {
        let got = events(&[b": ping\n\ndata: {\"a\":\ndata: 1}\n\n"]);
        assert_eq!(got, vec!["{\"a\":\n1}".to_string()]);
    }

    #[test]
    fn sse_handles_crlf_and_lines_split_across_chunks() {
        let got = events(&[
            b"event: message\r",
            b"\ndata: one\r\n",
            b"\r\ndata:two\r\n\r\n",
        ]);
        assert_eq!(got, vec!["one".to_string(), "two".to_string()]);
    }

    #[test]
    fn sse_event_without_data_is_not_dispatched() {
        assert!(events(&[b"event: ping\n\n"]).is_empty());
    }

    #[test]
    fn env_templates_expand_and_unset_names_the_variable() {
        let lookup = |name: &str| (name == "H9D_TOKEN").then(|| "s3cret".to_string());
        assert_eq!(
            expand_env_templates("Authorization", "Bearer ${H9D_TOKEN}", lookup).unwrap(),
            "Bearer s3cret"
        );
        assert_eq!(
            expand_env_templates("X", "plain $HOME", lookup).unwrap(),
            "plain $HOME"
        );
        let err =
            expand_env_templates("Authorization", "Bearer ${H9D_MISSING}", lookup).unwrap_err();
        assert!(err.contains("H9D_MISSING"), "{err}");
        assert!(!err.contains("s3cret"));
        assert!(expand_env_templates("X", "${oops", lookup).is_err());
        assert!(expand_env_templates("X", "${a-b}", lookup).is_err());
    }
}
