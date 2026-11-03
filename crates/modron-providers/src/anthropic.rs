//! Anthropic Messages API provider — LEDGER item P1.
//!
//! Non-streaming `POST {base_url}/v1/messages` with `x-api-key` and
//! `anthropic-version: 2023-06-01` headers. Maps the crate-agnostic
//! [`TurnRequest`]/[`TurnResponse`] types to/from Anthropic content blocks
//! (text, tool_use, tool_result) and usage (incl. `cache_read_input_tokens`).
//!
//! Request building ([`build_body`]) and response parsing ([`parse_response`])
//! are pure functions so tests assert the exact wire shape from recorded-JSON
//! fixtures — no network.

use crate::stream::{parse_sse_stream, StreamDelta};
use async_trait::async_trait;
use futures::StreamExt;
use modron_core::{
    Error, ModelProvider, Result, Role, StopReason, ToolCall, TurnRequest, TurnResponse, Usage,
};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

/// Anthropic provider id ([`ModelProvider::id`]).
pub const PROVIDER_ID: &str = "anthropic";

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Anthropic Messages API client.
pub struct AnthropicProvider {
    auth: std::sync::RwLock<Auth>,
    base_url: String,
    http: Client,
    /// Single-flight guard for subscription refreshes: refresh tokens
    /// rotate, so two concurrent 401s must not both POST the same
    /// single-use token (the loser's rotation kills the sign-in).
    refresh_lock: tokio::sync::Mutex<()>,
}

/// How the provider authenticates.
enum Auth {
    /// Pay-per-token API key (`x-api-key`).
    ApiKey(String),
    /// Claude-subscription OAuth (tokens from `/auth` → Anthropic →
    /// sign in with browser): `Authorization: Bearer` + the oauth beta
    /// header, and the server-validated Claude Code system-prompt
    /// prefix. Access tokens live ~8h; refresh tokens ROTATE on every
    /// refresh (the response's new one must be persisted).
    Subscription {
        access_token: String,
        refresh_token: Option<String>,
    },
}

/// Beta flags the Messages API requires for subscription (OAuth) traffic.
const OAUTH_BETA: &str = "oauth-2025-04-20,claude-code-20250219";
/// The Messages API validates that subscription requests carry a system
/// prompt starting with this sentence.
const CLAUDE_CODE_SYSTEM_PREFIX: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

impl AnthropicProvider {
    /// Create a client against the public API (`https://api.anthropic.com`).
    /// Fails only if the HTTP client cannot be built (TLS backend init).
    pub fn new(api_key: impl Into<String>) -> Result<Self> {
        Ok(Self {
            auth: std::sync::RwLock::new(Auth::ApiKey(api_key.into())),
            base_url: DEFAULT_BASE_URL.to_string(),
            http: build_client()?,
            refresh_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Create a client riding a Claude-subscription sign-in (tokens from
    /// `/auth`; see [`Auth::Subscription`]). ToS-gray and experimental:
    /// API keys remain the first-class path.
    pub fn from_subscription(
        access_token: impl Into<String>,
        refresh_token: Option<String>,
    ) -> Result<Self> {
        Ok(Self {
            auth: std::sync::RwLock::new(Auth::Subscription {
                access_token: access_token.into(),
                refresh_token,
            }),
            base_url: DEFAULT_BASE_URL.to_string(),
            http: build_client()?,
            refresh_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Whether the subscription (OAuth) auth mode is active.
    fn is_subscription(&self) -> bool {
        matches!(
            &*self.auth.read().expect("auth lock"),
            Auth::Subscription { .. }
        )
    }

    /// The system-prefix requirement for this auth mode (subscription
    /// only; empty = none).
    fn system_prefix(&self) -> &'static str {
        if self.is_subscription() {
            CLAUDE_CODE_SYSTEM_PREFIX
        } else {
            ""
        }
    }

    /// Send one `/v1/messages` request with the mode's headers; a 401 in
    /// subscription mode triggers one refresh-and-retry (refresh tokens
    /// rotate — the new pair is persisted before the retry). A failed
    /// refresh does not mask the original 401: the server's diagnostic is
    /// what the caller needs to see (revoked token, expired grant).
    async fn post_messages(&self, url: &str, body: &Value) -> Result<reqwest::Response> {
        let mut response = self.send_messages(url, body).await?;
        if response.status().as_u16() == 401 {
            match self.refresh_if_possible().await {
                Ok(true) => response = self.send_messages(url, body).await?,
                Ok(false) => {}
                Err(refresh_err) => {
                    tracing::warn!("{refresh_err:#} — surfacing the original 401");
                }
            }
        }
        Ok(response)
    }

    async fn send_messages(&self, url: &str, body: &Value) -> Result<reqwest::Response> {
        // Header values are extracted under a scoped guard: the future
        // must be Send, so no guard may live across the await.
        let (bearer, api_key) = {
            let auth = self.auth.read().expect("auth lock");
            match &*auth {
                Auth::ApiKey(key) => (None, Some(key.clone())),
                Auth::Subscription { access_token, .. } => (Some(access_token.clone()), None),
            }
        };
        let request = self
            .http
            .post(url)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(body);
        let request = match (bearer, api_key) {
            (Some(token), _) => request
                .header("Authorization", format!("Bearer {token}"))
                .header("anthropic-beta", OAUTH_BETA),
            (None, Some(key)) => request.header("x-api-key", key),
            (None, None) => unreachable!("auth is always one of the two"),
        };
        request
            .send()
            .await
            .map_err(|err| crate::transport_error(PROVIDER_ID, err))
    }

    /// Refresh an expired subscription access token. True when the tokens
    /// were rotated (persisted) and the caller should retry. The whole
    /// read-token → POST → persist → swap sequence runs under
    /// [`Self::refresh_lock`]: concurrent 401s re-read the (possibly
    /// already-rotated) token inside the lock instead of racing the same
    /// single-use token onto the wire twice.
    async fn refresh_if_possible(&self) -> Result<bool> {
        use modron_core::auth::oauth;
        let _guard = self.refresh_lock.lock().await;
        let refresh_token = {
            let auth = self.auth.read().expect("auth lock");
            match &*auth {
                Auth::Subscription {
                    refresh_token: Some(token),
                    ..
                } => token.clone(),
                _ => return Ok(false),
            }
        };
        let tokens = oauth::refresh_tokens_at(oauth::TOKEN_URL, &refresh_token)
            .await
            .map_err(|e| Error::Provider {
                provider: PROVIDER_ID.into(),
                message: format!("subscription token refresh failed: {e}"),
            })?;
        // Persist BEFORE swapping in memory: rotation means a crash must
        // never leave only the dead refresh token on disk.
        oauth::save_tokens(&tokens).map_err(|e| Error::Provider {
            provider: PROVIDER_ID.into(),
            message: format!("cannot persist refreshed tokens: {e}"),
        })?;
        *self.auth.write().expect("auth lock") = Auth::Subscription {
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
        };
        Ok(true)
    }

    /// Point the client at another deployment (proxy, gateway, Bedrock-style
    /// endpoint). The base URL has no trailing path: the `/v1/messages` suffix
    /// is appended per request.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Configured base URL (for diagnostics and factory tests).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Streaming variant of [`ModelProvider::turn`] — LEDGER item S1
    /// (inherent method; deliberately NOT on the [`ModelProvider`] trait).
    ///
    /// POSTs the same `/v1/messages` body plus `"stream": true` and decodes
    /// the Anthropic SSE protocol:
    /// `message_start` (input usage), `content_block_start` (tool_use →
    /// accumulate id/name), `content_block_delta` (`text_delta` →
    /// [`StreamDelta::Text`], `input_json_delta` → accumulate
    /// `partial_json`, `thinking_delta` → [`StreamDelta::Reasoning`]),
    /// `content_block_stop`, `message_delta` (`stop_reason` + output
    /// usage), `message_stop`.
    ///
    /// The returned [`TurnResponse`] must equal — field for field — what
    /// [`parse_response`] produces for the equivalent non-streaming body:
    /// same text (text blocks joined with `\n`), tool_calls with fully
    /// accumulated and parsed JSON arguments, usage, stop_reason. Thinking
    /// is streamed via [`StreamDelta::Reasoning`] and excluded from the
    /// response, exactly like the non-streaming path ignores thinking
    /// blocks. `StreamDelta::Done` is sent after the last delta, only on
    /// success.
    ///
    /// `tx` is a best-effort observer: sends to a dropped receiver are
    /// ignored (the authoritative result is the returned [`TurnResponse`]).
    ///
    /// NOT auto-retried: a retried stream would re-emit deltas to `tx`.
    /// Transport failures use the same message-prefix taxonomy as
    /// [`crate::retry::is_retryable`] (`timeout: …`, `connection error: …`,
    /// `request failed: …`) so the policy can be decided above this layer.
    /// A stream that ends without `message_stop` is a truncated response and
    /// fails with a `connection error: …` prefix rather than a partial
    /// result.
    async fn turn_streaming_inner(
        &self,
        req: TurnRequest,
        tx: UnboundedSender<StreamDelta>,
    ) -> Result<TurnResponse> {
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let mut body = build_body_with_prefix(&req, self.system_prefix());
        body["stream"] = json!(true);
        tracing::debug!(
            provider = PROVIDER_ID,
            model = %req.model,
            url = %url,
            messages = req.messages.len(),
            body_bytes = body.to_string().len(),
            "sending streaming turn request"
        );

        let response = self.post_messages(&url, &body).await?;
        let status = response.status();
        if !status.is_success() {
            // Streaming failures arrive as a plain (non-SSE) error body.
            let text = response
                .text()
                .await
                .map_err(|err| crate::transport_error(PROVIDER_ID, err))?;
            return Err(crate::http_error(PROVIDER_ID, status, &text));
        }

        let mut buffer: Vec<u8> = Vec::new();
        let mut byte_stream = response.bytes_stream();
        let mut acc = StreamAccumulator::default();
        while let Some(item) = byte_stream.next().await {
            let chunk = item.map_err(|err| crate::transport_error(PROVIDER_ID, err))?;
            buffer.extend_from_slice(&chunk);
            let (payloads, consumed) = parse_sse_stream(&buffer);
            buffer.drain(..consumed);
            for payload in payloads {
                acc.ingest(&payload, &tx)?;
            }
        }
        let parsed = acc.finish()?;
        tracing::debug!(
            provider = PROVIDER_ID,
            stop = ?parsed.stop_reason,
            input_tokens = parsed.usage.input_tokens,
            output_tokens = parsed.usage.output_tokens,
            tool_calls = parsed.tool_calls.len(),
            "streaming turn complete"
        );
        // Success: the last delta the consumer sees for this turn.
        let _ = tx.send(StreamDelta::Done);
        Ok(parsed)
    }
}

fn build_client() -> Result<Client> {
    Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .map_err(|err| Error::Other(anyhow::anyhow!("failed to build HTTP client: {err}")))
}

/// Incremental state for one streaming turn. Reconstructs exactly what
/// [`parse_response`] would produce for the equivalent complete body:
/// per-content-block accumulators keyed by the protocol's block `index`,
/// text blocks joined with `\n` in block order, tool arguments accumulated
/// as a JSON string and parsed at the end.
#[derive(Default)]
struct StreamAccumulator {
    /// `(block index, accumulated text)` for `text` content blocks.
    text_blocks: Vec<(u64, String)>,
    /// `(block index, id, name, accumulated partial_json)` for `tool_use`
    /// blocks, in first-appearance order.
    tool_blocks: Vec<(u64, String, String, String)>,
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    stop_reason: Option<String>,
    saw_message_start: bool,
    saw_stop: bool,
}

impl StreamAccumulator {
    /// Ingest one SSE `data:` payload (a single decoded Anthropic stream
    /// event). Unknown event types and delta types are ignored
    /// (forward-compatible), matching how [`parse_response`] skips unknown
    /// content blocks.
    fn ingest(&mut self, payload: &str, tx: &UnboundedSender<StreamDelta>) -> Result<()> {
        let event: Value = serde_json::from_str(payload).map_err(|err| Error::Provider {
            provider: PROVIDER_ID.into(),
            message: format!("malformed stream event: {err}"),
        })?;
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                self.saw_message_start = true;
                if let Some(usage) = event.pointer("/message/usage") {
                    if let Some(n) = usage.get("input_tokens").and_then(Value::as_u64) {
                        self.input_tokens = n;
                    }
                    if let Some(n) = usage.get("cache_read_input_tokens").and_then(Value::as_u64) {
                        self.cached_input_tokens = n;
                    }
                }
            }
            Some("content_block_start") => {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                let block = event.get("content_block").cloned().unwrap_or(Value::Null);
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => self.text_blocks.push((index, String::new())),
                    Some("tool_use") => {
                        let id = block
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .into();
                        let name = block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .into();
                        self.tool_blocks.push((index, id, name, String::new()));
                    }
                    // thinking / redacted_thinking / unknown blocks carry
                    // nothing that belongs in the TurnResponse.
                    _ => {}
                }
            }
            Some("content_block_delta") => {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                let delta = event.get("delta").cloned().unwrap_or(Value::Null);
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        let text = delta
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if let Some(slot) = self.text_blocks.iter_mut().find(|(i, _)| *i == index) {
                            slot.1.push_str(text);
                        }
                        if !text.is_empty() {
                            let _ = tx.send(StreamDelta::Text(text.to_string()));
                        }
                    }
                    Some("input_json_delta") => {
                        let fragment = delta
                            .get("partial_json")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if let Some(slot) = self.tool_blocks.iter_mut().find(|(i, ..)| *i == index)
                        {
                            slot.3.push_str(fragment);
                        }
                    }
                    Some("thinking_delta") => {
                        let thinking = delta
                            .get("thinking")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if !thinking.is_empty() {
                            let _ = tx.send(StreamDelta::Reasoning(thinking.to_string()));
                        }
                    }
                    // signature_delta and friends are bookkeeping only.
                    _ => {}
                }
            }
            Some("content_block_stop") => {
                // The block's arguments are COMPLETE: emit the call now so
                // the engine can speculatively execute idempotent read
                // tools while later blocks still stream. finish() re-parses
                // from tool_blocks for the final TurnResponse (unchanged).
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                if let Some((_, id, name, partial_json)) =
                    self.tool_blocks.iter().find(|(i, ..)| *i == index).cloned()
                {
                    // Empty arguments are a legal no-arg call; malformed
                    // JSON degrades to null exactly like finish() does.
                    // Mirror finish(): empty partial_json is a no-arg call;
                    // MALFORMED json is skipped here (no speculation on a
                    // call the turn is about to fail on) — finish() errors
                    // the whole turn for it.
                    let raw_args = partial_json.trim();
                    let arguments = if raw_args.is_empty() {
                        serde_json::json!({})
                    } else {
                        match serde_json::from_str::<Value>(raw_args) {
                            Ok(value) => value,
                            Err(_) => return Ok(()),
                        }
                    };
                    let _ = tx.send(StreamDelta::ToolCallReady(modron_core::ToolCall {
                        id,
                        name,
                        arguments,
                    }));
                }
            }
            Some("message_delta") => {
                if let Some(reason) = event.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop_reason = Some(reason.into());
                }
                if let Some(n) = event
                    .pointer("/usage/output_tokens")
                    .and_then(Value::as_u64)
                {
                    self.output_tokens = n;
                }
            }
            Some("message_stop") => self.saw_stop = true,
            Some("ping") => {}
            Some("error") => {
                let kind = event
                    .pointer("/error/type")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let message = event
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("upstream stream error");
                return Err(Error::Provider {
                    provider: PROVIDER_ID.into(),
                    message: format!("request failed: upstream stream error ({kind}): {message}"),
                });
            }
            _ => {}
        }
        Ok(())
    }

    /// Assemble the final [`TurnResponse`] once the byte stream has ended.
    fn finish(mut self) -> Result<TurnResponse> {
        if !self.saw_stop {
            return Err(Error::Provider {
                provider: PROVIDER_ID.into(),
                message: "connection error: stream ended before message_stop".into(),
            });
        }
        if !self.saw_message_start {
            // Usage is ground truth (PLAN §3.2); a stream without
            // message_start usage fails rather than reporting zeros.
            return Err(Error::Provider {
                provider: PROVIDER_ID.into(),
                message: "malformed stream: missing message_start usage".into(),
            });
        }

        self.text_blocks.sort_by_key(|(index, _)| *index);
        let mut text = String::new();
        for (_, block_text) in &self.text_blocks {
            // Same join semantics as parse_response: a `\n` separator
            // whenever the accumulated text is non-empty.
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(block_text);
        }

        self.tool_blocks.sort_by_key(|(index, ..)| *index);
        let mut tool_calls = Vec::new();
        for (_, id, name, partial_json) in &self.tool_blocks {
            let arguments = if partial_json.is_empty() {
                json!({})
            } else {
                serde_json::from_str(partial_json).map_err(|err| Error::Provider {
                    provider: PROVIDER_ID.into(),
                    message: format!("malformed tool arguments from model: {err}"),
                })?
            };
            tool_calls.push(ToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments,
            });
        }

        Ok(TurnResponse {
            text,
            tool_calls,
            usage: Usage {
                input_tokens: self.input_tokens,
                output_tokens: self.output_tokens,
                cached_input_tokens: self.cached_input_tokens,
            },
            stop_reason: map_stop_reason(self.stop_reason.as_deref()),
        })
    }
}

/// Build the `/v1/messages` request body from a [`TurnRequest`] (no
/// system prefix — see [`build_body_with_prefix`]).
pub fn build_body(req: &TurnRequest) -> Value {
    build_body_with_prefix(req, "")
}

/// [`build_body`] with a system-prompt prefix. Subscription (OAuth)
/// requests MUST carry a system prompt beginning with the Claude Code
/// sentence — the API validates it server-side; an empty prefix keeps
/// `req.system` verbatim (and skips the clone).
///
/// Mapping rules:
/// - `system` becomes a top-level `system` array of one text block carrying
///   `cache_control: {"type": "ephemeral"}` (omitted when empty) — prompt
///   cache breakpoint #1 (PLAN.md §4.11).
/// - `Role::User` → a user message with one text block.
/// - `Role::Assistant` → an assistant message with a text block (when
///   non-empty) plus one `tool_use` block per tool call.
/// - `Role::Tool` → a user message with a `tool_result` block; consecutive
///   same-role messages are merged (the API requires strictly alternating
///   roles).
/// - each [`ToolSpec`](modron_core::ToolSpec) → `{"name", "description",
///   "input_schema"}`; the LAST tool also carries
///   `cache_control: {"type": "ephemeral"}` — prompt cache breakpoint #2,
///   marking the end of the cacheable tools+system prefix.
pub fn build_body_with_prefix(req: &TurnRequest, system_prefix: &str) -> Value {
    if system_prefix.is_empty() {
        return build_body_inner(req);
    }
    let mut req = req.clone();
    req.system = if req.system.is_empty() {
        system_prefix.to_string()
    } else {
        format!("{system_prefix}\n\n{}", req.system)
    };
    build_body_inner(&req)
}

fn build_body_inner(req: &TurnRequest) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    let mut last_role: Option<&'static str> = None;

    for message in &req.messages {
        let (role, blocks): (&'static str, Vec<Value>) = match message.role {
            Role::User => {
                // Multimodal (Phase 7): attachments become content blocks
                // BEFORE the text — Anthropic image blocks are base64
                // sources; documents inline as text blocks.
                let mut blocks: Vec<Value> = Vec::new();
                for attachment in &message.attachments {
                    match attachment {
                        modron_core::Attachment::Image { media_type, data } => {
                            blocks.push(json!({
                                "type": "image",
                                "source": {
                                    "type": "base64",
                                    "media_type": media_type,
                                    "data": data,
                                }
                            }));
                        }
                        modron_core::Attachment::Document { name, text } => {
                            blocks.push(json!({
                                "type": "text",
                                "text": format!("[Attached document: {name}]\n{text}")
                            }));
                        }
                    }
                }
                blocks.push(json!({
                    "type": "text",
                    "text": non_empty(&message.content, "(empty message)")
                }));
                ("user", blocks)
            }
            Role::Assistant => {
                let mut blocks = Vec::new();
                if !message.content.is_empty() {
                    blocks.push(json!({ "type": "text", "text": message.content }));
                }
                for call in &message.tool_calls {
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": call.id,
                        "name": call.name,
                        "input": call.arguments,
                    }));
                }
                if blocks.is_empty() {
                    blocks.push(json!({ "type": "text", "text": "(empty assistant message)" }));
                }
                ("assistant", blocks)
            }
            Role::Tool => (
                "user",
                vec![json!({
                    "type": "tool_result",
                    "tool_use_id": message.tool_call_id.clone().unwrap_or_default(),
                    "content": non_empty(&message.content, "(empty result)"),
                })],
            ),
        };

        if last_role == Some(role) {
            if let Some(last) = messages.last_mut() {
                if let Some(existing) = last.get_mut("content").and_then(Value::as_array_mut) {
                    existing.extend(blocks);
                }
            }
        } else {
            messages.push(json!({ "role": role, "content": blocks }));
            last_role = Some(role);
        }
    }

    let mut body = json!({
        "model": req.model,
        "max_tokens": req.max_tokens,
        "messages": messages,
    });
    if !req.system.is_empty() {
        body["system"] = json!([{
            "type": "text",
            "text": req.system,
            "cache_control": { "type": "ephemeral" },
        }]);
    }
    if !req.tools.is_empty() {
        let mut tools: Vec<Value> = req
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.parameters,
                })
            })
            .collect();
        // Prompt cache breakpoint #2: only the last tool definition may carry
        // cache_control (Anthropic allows at most 4 breakpoints; marking the
        // last tool caches the entire tools+system prefix).
        if let Some(last) = tools.last_mut() {
            last["cache_control"] = json!({ "type": "ephemeral" });
        }
        body["tools"] = Value::Array(tools);
    }
    body
}

/// Parse a `/v1/messages` response body into a [`TurnResponse`].
///
/// Unknown content-block types (e.g. `thinking`) are skipped. `usage` is
/// required — provider-reported usage is ground truth for accounting, so a
/// response without it is a protocol error, not a zero.
pub fn parse_response(body: Value) -> Result<TurnResponse> {
    let resp: AnthropicResponse = serde_json::from_value(body).map_err(|err| Error::Provider {
        provider: PROVIDER_ID.into(),
        message: format!("malformed response body: {err}"),
    })?;

    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for block in resp.content {
        match block {
            ContentBlock::Text { text: chunk } => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&chunk);
            }
            ContentBlock::ToolUse { id, name, input } => {
                tool_calls.push(ToolCall {
                    id,
                    name,
                    arguments: if input.is_null() { json!({}) } else { input },
                });
            }
            ContentBlock::Other => {}
        }
    }

    let usage = Usage {
        input_tokens: resp.usage.input_tokens,
        output_tokens: resp.usage.output_tokens,
        cached_input_tokens: resp.usage.cache_read_input_tokens.unwrap_or(0),
    };
    Ok(TurnResponse {
        text,
        tool_calls,
        usage,
        stop_reason: map_stop_reason(resp.stop_reason.as_deref()),
    })
}

fn map_stop_reason(reason: Option<&str>) -> StopReason {
    match reason {
        Some("end_turn") => StopReason::EndTurn,
        Some("tool_use") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::MaxTokens,
        _ => StopReason::Other,
    }
}

fn non_empty<'a>(s: &'a str, fallback: &'a str) -> &'a str {
    if s.is_empty() {
        fallback
    } else {
        s
    }
}

#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    content: Vec<ContentBlock>,
    #[serde(default)]
    stop_reason: Option<String>,
    usage: AnthropicUsage,
}

#[derive(Debug, Deserialize)]
struct AnthropicUsage {
    input_tokens: u64,
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
}

/// Known content-block shapes; anything else (e.g. `thinking`) is ignored.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: Value,
    },
    #[serde(other)]
    Other,
}

#[async_trait]
impl ModelProvider for AnthropicProvider {
    /// Trait-level streaming override (S1 lift): delegates to the inherent
    /// implementation so `dyn ModelProvider` dispatch streams, not just
    /// concrete-type callers.
    async fn turn_streaming(
        &self,
        req: TurnRequest,
        tx: tokio::sync::mpsc::UnboundedSender<StreamDelta>,
    ) -> Result<TurnResponse> {
        self.turn_streaming_inner(req, tx).await
    }
    fn id(&self) -> &str {
        PROVIDER_ID
    }

    async fn turn(&self, req: TurnRequest) -> Result<TurnResponse> {
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let body = build_body_with_prefix(&req, self.system_prefix());
        // Log shapes and sizes only — never the API key or full body.
        tracing::debug!(
            provider = PROVIDER_ID,
            model = %req.model,
            url = %url,
            messages = req.messages.len(),
            body_bytes = body.to_string().len(),
            "sending turn request"
        );

        let response = self.post_messages(&url, &body).await?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|err| crate::transport_error(PROVIDER_ID, err))?;
        if !status.is_success() {
            return Err(crate::http_error(PROVIDER_ID, status, &text));
        }

        let value: Value = serde_json::from_str(&text).map_err(|err| Error::Provider {
            provider: PROVIDER_ID.into(),
            message: format!("malformed response: {err}"),
        })?;
        let parsed = parse_response(value)?;
        tracing::debug!(
            provider = PROVIDER_ID,
            stop = ?parsed.stop_reason,
            input_tokens = parsed.usage.input_tokens,
            output_tokens = parsed.usage.output_tokens,
            tool_calls = parsed.tool_calls.len(),
            "turn complete"
        );
        Ok(parsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use modron_core::{Message, ToolOutcome, ToolSpec};
    use serde_json::json;

    #[test]
    fn subscription_mode_carries_the_required_system_prefix() {
        // Subscription (OAuth) requests are server-validated to carry a
        // system prompt beginning with the Claude Code sentence.
        let provider = AnthropicProvider::from_subscription("at-token", Some("rt".into())).unwrap();
        assert!(provider.is_subscription());
        let body = build_body_with_prefix(&sample_request(), provider.system_prefix());
        let system = body["system"].as_array().unwrap();
        let text = system[0]["text"].as_str().unwrap();
        assert!(
            text.starts_with("You are Claude Code, Anthropic's official CLI for Claude."),
            "{text}"
        );
        // The original system prompt survives after the prefix.
        assert!(text.ends_with("You are Modron."), "{text}");

        // API-key mode: no prefix, verbatim system.
        let key_provider = AnthropicProvider::new("sk").unwrap();
        assert!(!key_provider.is_subscription());
        assert_eq!(key_provider.system_prefix(), "");
        let body = build_body_with_prefix(&sample_request(), key_provider.system_prefix());
        assert_eq!(
            body["system"][0]["text"].as_str().unwrap(),
            "You are Modron."
        );
    }

    fn sample_request() -> TurnRequest {
        TurnRequest {
            model: "claude-sonnet-4-5".into(),
            system: "You are Modron.".into(),
            messages: vec![
                Message::user("list the rust files"),
                {
                    let mut message = Message::assistant("Checking.");
                    message.tool_calls = vec![ToolCall {
                        id: "toolu_1".into(),
                        name: "glob".into(),
                        arguments: json!({"pattern": "*.rs"}),
                    }];
                    message
                },
                Message::tool_result("toolu_1", &ToolOutcome::ok("main.rs\nlib.rs")),
            ],
            tools: vec![ToolSpec {
                name: "glob".into(),
                description: "Find files by pattern.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {"pattern": {"type": "string"}},
                    "required": ["pattern"]
                }),
            }],
            max_tokens: 8192,
        }
    }

    #[test]
    fn attachments_encode_as_image_blocks() {
        let req = TurnRequest {
            model: "claude-sonnet-4-5".into(),
            system: String::new(),
            messages: vec![modron_core::Message::user_with_attachments(
                "what is this?",
                vec![modron_core::Attachment::Image {
                    media_type: "image/png".into(),
                    data: "aGVsbG8=".into(),
                }],
            )],
            tools: Vec::new(),
            max_tokens: 128,
        };
        let body = build_body(&req);
        let content = &body["messages"][0]["content"];
        assert_eq!(content[0]["type"], "image");
        assert_eq!(content[0]["source"]["type"], "base64");
        assert_eq!(content[0]["source"]["media_type"], "image/png");
        assert_eq!(content[0]["source"]["data"], "aGVsbG8=");
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "what is this?");
    }

    #[test]
    fn documents_inline_as_text_blocks() {
        let req = TurnRequest {
            model: "claude-sonnet-4-5".into(),
            system: String::new(),
            messages: vec![modron_core::Message::user_with_attachments(
                "summarize",
                vec![modron_core::Attachment::Document {
                    name: "notes.md".into(),
                    text: "hello notes".into(),
                }],
            )],
            tools: Vec::new(),
            max_tokens: 128,
        };
        let body = build_body(&req);
        let content = &body["messages"][0]["content"];
        assert!(content[0]["text"]
            .as_str()
            .unwrap()
            .contains("[Attached document: notes.md]"));
        assert!(content[0]["text"].as_str().unwrap().contains("hello notes"));
    }

    #[test]
    fn request_body_shape() {
        let body = build_body(&sample_request());
        assert_eq!(body["model"], "claude-sonnet-4-5");
        assert_eq!(body["max_tokens"], 8192);
        // System is a one-block array with the cache breakpoint (K1), not a
        // bare string.
        let system = body["system"].as_array().unwrap();
        assert_eq!(system.len(), 1);
        assert_eq!(system[0]["type"], "text");
        assert_eq!(system[0]["text"], "You are Modron.");
        assert_eq!(system[0]["cache_control"]["type"], "ephemeral");

        let messages = body["messages"].as_array().unwrap();
        assert_eq!(
            messages.len(),
            3,
            "roles must alternate user/assistant/user"
        );
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"][0]["type"], "text");
        assert_eq!(messages[0]["content"][0]["text"], "list the rust files");

        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"][0]["type"], "text");
        assert_eq!(messages[1]["content"][0]["text"], "Checking.");
        assert_eq!(messages[1]["content"][1]["type"], "tool_use");
        assert_eq!(messages[1]["content"][1]["id"], "toolu_1");
        assert_eq!(messages[1]["content"][1]["name"], "glob");
        assert_eq!(messages[1]["content"][1]["input"]["pattern"], "*.rs");

        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
        assert_eq!(messages[2]["content"][0]["tool_use_id"], "toolu_1");
        assert_eq!(messages[2]["content"][0]["content"], "main.rs\nlib.rs");

        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "glob");
        assert_eq!(tools[0]["description"], "Find files by pattern.");
        assert_eq!(tools[0]["input_schema"]["required"][0], "pattern");
        // With one tool it is the last (and only) tool → carries the second
        // cache breakpoint.
        assert_eq!(tools[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn cache_control_appears_exactly_twice_on_last_tool_only() {
        let mut req = sample_request();
        req.tools.push(ToolSpec {
            name: "read".into(),
            description: "Read a file.".into(),
            parameters: json!({"type": "object"}),
        });
        req.tools.push(ToolSpec {
            name: "grep".into(),
            description: "Search files.".into(),
            parameters: json!({"type": "object"}),
        });
        let body = build_body(&req);

        // Exactly two breakpoints in the whole body: system block + last tool.
        let serialized = body.to_string();
        assert_eq!(
            serialized.matches("cache_control").count(),
            2,
            "body must carry exactly two cache_control breakpoints: {serialized}"
        );

        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 3);
        assert!(
            tools[0].get("cache_control").is_none(),
            "only the LAST tool is marked"
        );
        assert!(
            tools[1].get("cache_control").is_none(),
            "only the LAST tool is marked"
        );
        assert_eq!(tools[2]["name"], "grep");
        assert_eq!(tools[2]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn empty_system_keeps_tools_breakpoint_and_omits_system() {
        let mut req = sample_request();
        req.system = String::new();
        req.tools.push(ToolSpec {
            name: "read".into(),
            description: "Read a file.".into(),
            parameters: json!({"type": "object"}),
        });
        let body = build_body(&req);

        assert!(body.get("system").is_none(), "empty system stays omitted");
        let tools = body["tools"].as_array().unwrap();
        assert!(tools[0].get("cache_control").is_none());
        assert_eq!(tools[1]["cache_control"]["type"], "ephemeral");
        assert_eq!(body.to_string().matches("cache_control").count(), 1);
    }

    #[test]
    fn system_without_tools_has_single_breakpoint() {
        let req = TurnRequest {
            model: "claude-sonnet-4-5".into(),
            system: "You are Modron.".into(),
            messages: vec![Message::user("hi")],
            tools: vec![],
            max_tokens: 16,
        };
        let body = build_body(&req);
        assert!(body.get("tools").is_none());
        let system = body["system"].as_array().unwrap();
        assert_eq!(system.len(), 1);
        assert_eq!(system[0]["type"], "text");
        assert_eq!(system[0]["text"], "You are Modron.");
        assert_eq!(system[0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body.to_string().matches("cache_control").count(), 1);
    }

    #[test]
    fn consecutive_tool_results_merge_into_one_user_message() {
        let mut req = sample_request();
        req.messages
            .push(Message::tool_result("toolu_2", &ToolOutcome::err("boom")));
        let body = build_body(&req);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[2]["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn empty_system_and_tools_are_omitted() {
        let req = TurnRequest {
            model: "claude-sonnet-4-5".into(),
            system: String::new(),
            messages: vec![Message::user("hi")],
            tools: vec![],
            max_tokens: 16,
        };
        let body = build_body(&req);
        assert!(body.get("system").is_none());
        assert!(body.get("tools").is_none());
        assert_eq!(
            body.to_string().matches("cache_control").count(),
            0,
            "no breakpoints without a system block or tools"
        );
    }

    #[test]
    fn parses_recorded_fixture_with_tool_use() {
        let fixture = r#"{
            "id": "msg_01ABC",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-5",
            "content": [
                {"type": "text", "text": "Let me check the repo."},
                {"type": "tool_use", "id": "toolu_01XYZ", "name": "glob", "input": {"pattern": "*.rs"}}
            ],
            "stop_reason": "tool_use",
            "stop_sequence": null,
            "usage": {"input_tokens": 2095, "output_tokens": 121, "cache_read_input_tokens": 1024}
        }"#;
        let parsed = parse_response(serde_json::from_str::<Value>(fixture).unwrap()).unwrap();

        assert_eq!(parsed.text, "Let me check the repo.");
        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].id, "toolu_01XYZ");
        assert_eq!(parsed.tool_calls[0].name, "glob");
        assert_eq!(parsed.tool_calls[0].arguments["pattern"], "*.rs");
        assert_eq!(parsed.stop_reason, StopReason::ToolUse);
        assert_eq!(parsed.usage.input_tokens, 2095);
        assert_eq!(parsed.usage.output_tokens, 121);
        assert_eq!(parsed.usage.cached_input_tokens, 1024);
    }

    #[test]
    fn skips_unknown_content_blocks_and_maps_stop_reasons() {
        let fixture = r#"{
            "content": [
                {"type": "thinking", "thinking": "hmm", "signature": "sig"},
                {"type": "text", "text": "Done."}
            ],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        }"#;
        let parsed = parse_response(serde_json::from_str::<Value>(fixture).unwrap()).unwrap();
        assert_eq!(parsed.text, "Done.");
        assert!(parsed.tool_calls.is_empty());
        assert_eq!(parsed.stop_reason, StopReason::EndTurn);
        assert_eq!(parsed.usage.cached_input_tokens, 0);
    }

    #[test]
    fn max_tokens_stop_reason_maps() {
        let fixture = r#"{
            "content": [{"type": "text", "text": "truncat"}],
            "stop_reason": "max_tokens",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }"#;
        let parsed = parse_response(serde_json::from_str::<Value>(fixture).unwrap()).unwrap();
        assert_eq!(parsed.stop_reason, StopReason::MaxTokens);
    }

    #[test]
    fn tool_use_with_null_input_becomes_empty_object() {
        let fixture = r#"{
            "content": [{"type": "tool_use", "id": "t1", "name": "bash", "input": null}],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }"#;
        let parsed = parse_response(serde_json::from_str::<Value>(fixture).unwrap()).unwrap();
        assert_eq!(parsed.tool_calls[0].arguments, json!({}));
    }

    #[test]
    fn missing_usage_is_a_protocol_error() {
        // Usage is ground truth for accounting — a response without it fails.
        let parsed = parse_response(json!({"content": [], "stop_reason": "end_turn"}));
        assert!(
            matches!(parsed, Err(Error::Provider { .. })),
            "got: {parsed:?}"
        );
    }

    #[test]
    fn base_url_defaults_and_overrides() {
        let provider = AnthropicProvider::new("sk-test").unwrap();
        assert_eq!(provider.base_url(), "https://api.anthropic.com");
        let overridden = provider.with_base_url("http://localhost:8082/");
        assert_eq!(overridden.base_url(), "http://localhost:8082/");
    }
}
