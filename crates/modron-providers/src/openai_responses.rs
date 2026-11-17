//! OpenAI Responses provider, in two modes that share the request and
//! stream code.
//!
//! - ChatGPT subscription ([`PROVIDER_ID`] = "chatgpt"): the Codex-style
//!   backend at `chatgpt.com/backend-api/codex/responses` speaks the
//!   Responses API (`instructions` + typed `input` items) and requires the
//!   `chatgpt-account-id` header alongside the OAuth bearer token. Login is
//!   the device flow in `modron_core::auth::oauth::chatgpt` (`/auth add` in
//!   the TUI); tokens live 0600 in `~/.modron/auth/openai.json`.
//! - API key ([`API_KEY_PROVIDER_ID`] = "openai-responses"): the public
//!   `POST {OPENAI_BASE_URL}/responses` (default `https://api.openai.com/v1`)
//!   with `Authorization: Bearer $OPENAI_API_KEY`. No account header, no
//!   refresh. Same body and stream shape.
//!
//! The two modes keep separate provider ids for reasoning-block matching
//! (H7b-3): the same wire format, but encrypted reasoning items are scoped to
//! the account or key that produced them, so a chatgpt block never replays to
//! an API-key turn or the reverse.
//!
//! Request building ([`build_body`]) and event decoding
//! ([`ResponsesAccumulator`]) are pure; tests assert wire shape and parse
//! scripted SSE fixtures — no network. Live verification needs a real
//! ChatGPT sign-in (the user's browser round-trip) or an OpenAI API key.

use async_trait::async_trait;
use modron_core::auth::oauth::chatgpt;
use modron_core::{
    replayable_blocks, Error, ModelProvider, ProviderBlock, Result, Role, StopReason, ToolCall,
    TurnRequest, TurnResponse, Usage,
};
use reqwest::Client;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc::UnboundedSender;

/// ChatGPT (subscription) provider id ([`ModelProvider::id`]).
pub const PROVIDER_ID: &str = "chatgpt";
/// API-key provider id ([`ModelProvider::id`]) for `OPENAI_API_KEY` traffic
/// to the public Responses endpoint.
pub const API_KEY_PROVIDER_ID: &str = "openai-responses";
/// Default API-key base URL (`/responses` is appended).
pub const API_KEY_DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
/// Wire api for provider-block replay (H7b-2): reasoning items are only
/// replayed to this api.
const REPLAY_API: &str = "responses";
/// Debug-only: when set to a directory, every outgoing request body is written
/// there as `NNNN-<provider>.json` (body only, never headers). Off by default.
const DEBUG_REQUEST_DIR_ENV: &str = "MODRON_DEBUG_REQUEST_DIR";
static DEBUG_REQUEST_SEQ: AtomicU64 = AtomicU64::new(0);

use crate::http::{
    build_client, check_status, drain_sse, with_401_retry, within_idle, REQUEST_TIMEOUT,
    STREAM_IDLE_TIMEOUT,
};
const DONE_SENTINEL: &str = "[DONE]";

/// Live credentials behind a lock: a successful refresh swaps them in
/// (persisted to disk first).
#[derive(Clone)]
struct StoredAuth {
    access_token: String,
    refresh_token: Option<String>,
    account_id: String,
}

/// Which credentials and endpoint a provider instance uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// ChatGPT subscription: OAuth bearer + `chatgpt-account-id`, full
    /// backend endpoint, token refresh on 401.
    Chatgpt,
    /// Public API: bearer API key, `{base}/responses`, no refresh.
    ApiKey,
}

/// A Responses API provider: the ChatGPT subscription backend, or the public
/// API with an API key (see the module doc).
pub struct OpenAiResponsesProvider {
    auth: std::sync::RwLock<StoredAuth>,
    /// ChatGPT: the full endpoint. API key: the `/v1` base (no trailing slash).
    base_url: String,
    http: Client,
    /// Single-flight guard for refreshes — concurrent 401s must not both
    /// POST the same single-use refresh token.
    refresh_lock: tokio::sync::Mutex<()>,
    mode: Mode,
    /// Block-matching and error provider id ([`PROVIDER_ID`] or
    /// [`API_KEY_PROVIDER_ID`]).
    provider_id: &'static str,
}

impl OpenAiResponsesProvider {
    /// Build from explicitly passed credentials.
    pub fn new(access_token: impl Into<String>, account_id: impl Into<String>) -> Result<Self> {
        Ok(Self {
            auth: std::sync::RwLock::new(StoredAuth {
                access_token: access_token.into(),
                refresh_token: None,
                account_id: account_id.into(),
            }),
            base_url: chatgpt::BACKEND_URL.to_string(),
            http: build_client()?,
            refresh_lock: tokio::sync::Mutex::new(()),
            mode: Mode::Chatgpt,
            provider_id: PROVIDER_ID,
        })
    }

    /// Build the API-key provider (`openai-responses`): `api_key` as the
    /// bearer, `base_url` as the `/v1` root (`/responses` is appended per turn).
    pub fn with_api_key(api_key: impl Into<String>, base_url: impl Into<String>) -> Result<Self> {
        Ok(Self {
            auth: std::sync::RwLock::new(StoredAuth {
                access_token: api_key.into(),
                refresh_token: None,
                account_id: String::new(),
            }),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            http: build_client()?,
            refresh_lock: tokio::sync::Mutex::new(()),
            mode: Mode::ApiKey,
            provider_id: API_KEY_PROVIDER_ID,
        })
    }

    /// Full POST endpoint for this mode.
    fn endpoint(&self) -> String {
        match self.mode {
            Mode::Chatgpt => self.base_url.trim_end_matches('/').to_string(),
            Mode::ApiKey => format!("{}/responses", self.base_url),
        }
    }

    /// Build from the stored `/auth add` sign-in. Actionable error when no
    /// sign-in exists. The refresh token rides along so an expired access
    /// token can be rotated transparently instead of hard-failing.
    pub fn from_stored_tokens() -> Result<Self> {
        match chatgpt::load() {
            Ok(Some(auth)) => Ok(Self {
                auth: std::sync::RwLock::new(StoredAuth {
                    access_token: auth.access_token,
                    refresh_token: auth.refresh_token,
                    account_id: auth.account_id,
                }),
                base_url: chatgpt::BACKEND_URL.to_string(),
                http: build_client()?,
                refresh_lock: tokio::sync::Mutex::new(()),
                mode: Mode::Chatgpt,
                provider_id: PROVIDER_ID,
            }),
            Ok(None) => Err(Error::Config(
                "no ChatGPT sign-in".into(),
                "run `/auth add` in the TUI (or `modron auth add`) and pick the ChatGPT \
                 sign-in row — it opens a browser device flow"
                    .into(),
            )),
            Err(e) => Err(Error::Config(
                "cannot read the stored ChatGPT tokens".into(),
                format!("{e} — re-run the sign-in to rewrite the file"),
            )),
        }
    }

    /// Configured backend URL (diagnostics + tests).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// One streaming turn against the Responses endpoint.
    async fn turn_streaming_inner(
        &self,
        req: &TurnRequest,
        tx: UnboundedSender<modron_core::StreamDelta>,
    ) -> Result<TurnResponse> {
        // ChatGPT: BACKEND_URL is the complete endpoint — no path to append
        // (double-appending /responses 404s with {"detail":"Not Found"}).
        // API key: the base is the /v1 root, so `/responses` is appended.
        let url = self.endpoint();
        let body = build_body_for(req, self.provider_id);
        debug_dump_request(self.provider_id, &body);
        let response = within_idle(
            self.provider_id,
            STREAM_IDLE_TIMEOUT,
            self.post(&url, &body),
        )
        .await?;
        // Streaming failures arrive as a plain (non-SSE) error body; the
        // shared status check maps them to the canonical error text.
        let response = check_status(self.provider_id, response).await?;

        let mut acc = ResponsesAccumulator::new(self.provider_id);
        drain_sse(
            self.provider_id,
            response.bytes_stream(),
            STREAM_IDLE_TIMEOUT,
            |payload| {
                if payload == DONE_SENTINEL {
                    acc.done = true;
                    return Ok(true);
                }
                acc.ingest(payload, &tx).map(|()| false)
            },
        )
        .await?;
        let parsed = acc.finish()?;
        let _ = tx.send(modron_core::StreamDelta::Done);
        Ok(parsed)
    }

    /// POST with the stored bearer; a 401 triggers one single-flight
    /// refresh-and-retry via the shared [`with_401_retry`] (anthropic
    /// shipped the same twin; 2026-11-12 audit DRY fix).
    async fn post(&self, url: &str, body: &Value) -> Result<reqwest::Response> {
        with_401_retry(|| self.send(url, body), || self.refresh_if_possible()).await
    }

    async fn send(&self, url: &str, body: &Value) -> Result<reqwest::Response> {
        // Credentials are cloned under a scoped guard: the future must be
        // Send, so no guard may live across the await.
        let (access_token, account_id) = {
            let auth = self.auth.read().expect("auth lock");
            (auth.access_token.clone(), auth.account_id.clone())
        };
        let request = self
            .http
            .post(url)
            .header("Authorization", format!("Bearer {access_token}"));
        // ChatGPT-backend-only headers; the public API takes the bearer alone.
        let request = match self.mode {
            Mode::Chatgpt => request
                .header("chatgpt-account-id", account_id)
                .header("OpenAI-Beta", "responses=experimental")
                .header("originator", "modron"),
            Mode::ApiKey => request,
        };
        request
            .json(body)
            .send()
            .await
            .map_err(|err| crate::transport_error(self.provider_id, err))
    }

    /// Refresh an expired access token via the OpenAI refresh-token grant.
    /// True when the tokens were rotated (persisted to disk BEFORE the
    /// in-memory swap) and the caller should retry. Runs entirely under
    /// [`Self::refresh_lock`]; the token is re-read inside the lock so a
    /// concurrent winner's rotation is picked up instead of replaying the
    /// now-dead token.
    async fn refresh_if_possible(&self) -> Result<bool> {
        // The access token the failed request used, read BEFORE waiting on
        // any lock (see the Anthropic provider: same single-use refresh
        // token hazard, same fix).
        let stale_access = self.auth.read().expect("auth lock").access_token.clone();
        let _guard = self.refresh_lock.lock().await;
        let (current_access, refresh_token) = {
            let auth = self.auth.read().expect("auth lock");
            (auth.access_token.clone(), auth.refresh_token.clone())
        };
        let Some(refresh_token) = refresh_token else {
            return Ok(false);
        };
        if current_access != stale_access {
            return Ok(true); // another task in this process refreshed
        }
        // Cross-process (E8): file lock, then re-read the token file and
        // adopt tokens another Modron process already rotated.
        let _file_lock = match chatgpt::token_file() {
            Ok(path) => Some(modron_core::auth::RefreshFileLock::acquire(&path).await),
            Err(_) => None,
        };
        if let Ok(Some(disk)) = chatgpt::load() {
            let rotated = disk.access_token != stale_access
                && disk
                    .refresh_token
                    .as_deref()
                    .is_some_and(|token| token != refresh_token);
            if rotated {
                *self.auth.write().expect("auth lock") = StoredAuth {
                    access_token: disk.access_token,
                    refresh_token: disk.refresh_token,
                    account_id: disk.account_id,
                };
                return Ok(true);
            }
        }
        let grant = refresh_grant(&self.http, chatgpt::TOKEN_URL, &refresh_token).await?;
        // OpenAI rotates refresh tokens; keep the old one only when the
        // response omits it.
        let new_refresh_token = grant.refresh_token.or(Some(refresh_token));
        let account_id = grant
            .account_id
            .unwrap_or_else(|| self.auth.read().expect("auth lock").account_id.clone());
        let new_auth = StoredAuth {
            access_token: grant.access_token,
            refresh_token: new_refresh_token,
            account_id: account_id.clone(),
        };
        // Persist BEFORE swapping in memory: a crash must never leave only
        // the dead refresh token on disk.
        chatgpt::save(&chatgpt::ChatGptAuth {
            access_token: new_auth.access_token.clone(),
            refresh_token: new_auth.refresh_token.clone(),
            account_id,
        })
        .map_err(|e| Error::Provider {
            provider: PROVIDER_ID.into(),
            message: format!("cannot persist refreshed tokens: {e}"),
        })?;
        *self.auth.write().expect("auth lock") = new_auth;
        Ok(true)
    }
}

#[async_trait]
impl ModelProvider for OpenAiResponsesProvider {
    fn id(&self) -> &str {
        self.provider_id
    }

    async fn turn(&self, req: std::sync::Arc<TurnRequest>) -> Result<TurnResponse> {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        self.turn_streaming_inner(&req, tx).await
    }

    async fn turn_streaming(
        &self,
        req: std::sync::Arc<TurnRequest>,
        tx: UnboundedSender<modron_core::StreamDelta>,
    ) -> Result<TurnResponse> {
        self.turn_streaming_inner(&req, tx).await
    }
}

/// Map a [`TurnRequest`] to the ChatGPT-provider body shape (see
/// [`build_body_for`]).
pub fn build_body(req: &TurnRequest) -> Value {
    build_body_for(req, PROVIDER_ID)
}

/// Write the request body to `$MODRON_DEBUG_REQUEST_DIR` when that is set
/// (debug only; errors are ignored). Only the JSON body is written.
fn debug_dump_request(provider_id: &str, body: &Value) {
    let Some(dir) = std::env::var_os(DEBUG_REQUEST_DIR_ENV).filter(|d| !d.is_empty()) else {
        return;
    };
    let seq = DEBUG_REQUEST_SEQ.fetch_add(1, Ordering::SeqCst);
    let path = std::path::Path::new(&dir).join(format!("{seq:04}-{provider_id}.json"));
    if let Ok(text) = serde_json::to_string_pretty(body) {
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(path, text);
    }
}

/// Map a [`TurnRequest`] to the Responses API body shape for `provider_id`:
/// `instructions` carries the system prompt and the transcript becomes typed
/// `input` items (message / function_call / function_call_output). Reasoning
/// blocks replay only when their provider id matches `provider_id`.
pub fn build_body_for(req: &TurnRequest, provider_id: &str) -> Value {
    let mut input: Vec<Value> = Vec::new();
    for message in &req.messages {
        match message.role {
            Role::User => {
                // Multimodal: images ride as input_image data URLs (the
                // Responses wire form of image_url parts); documents
                // inline as text parts, mirroring the openai-compat
                // encoding. This also unlocks bitmap-compaction payloads
                // for gpt-6/gpt-5.6 turns.
                let mut content = vec![json!({
                    "type": "input_text",
                    "text": message.content,
                })];
                for attachment in &message.attachments {
                    match attachment {
                        modron_core::Attachment::Image { media_type, data } => {
                            content.push(json!({
                                "type": "input_image",
                                "image_url": format!("data:{media_type};base64,{data}"),
                            }));
                        }
                        modron_core::Attachment::Document { name, text } => {
                            content.push(json!({
                                "type": "input_text",
                                "text": format!("[Attached document: {name}]\n{text}"),
                            }));
                        }
                    }
                }
                input.push(json!({
                    "type": "message",
                    "role": "user",
                    "content": content,
                }));
            }
            Role::Assistant => {
                // Reasoning items first, in their original order, exactly as
                // received (H7b-2); only this provider's Responses items.
                for block in replayable_blocks(message, provider_id, REPLAY_API) {
                    input.push(block.payload.clone());
                }
                if !message.content.is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": message.content}],
                    }));
                }
                for call in &message.tool_calls {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": call.id,
                        "name": call.name,
                        "arguments": call.arguments.to_string(),
                    }));
                }
            }
            Role::Tool => input.push(json!({
                "type": "function_call_output",
                "call_id": message.tool_call_id.clone().unwrap_or_default(),
                "output": message.content,
            })),
        }
    }
    let tools: Vec<Value> = req
        .tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "name": t.name,
                "description": t.description,
                "parameters": t.parameters,
                "strict": false,
            })
        })
        .collect();
    let mut body = json!({
        "model": req.model,
        "instructions": req.system,
        "input": input,
        "store": false,
        "stream": true,
    });
    // Reasoning on: ask for the encrypted reasoning items so they can be
    // sent back on the next turn (store is false, so this is the only copy).
    // Absent when reasoning is off: the request bytes are unchanged then.
    if req.effort.is_some() || req.reasoning_summary {
        body["include"] = json!(["reasoning.encrypted_content"]);
    }
    if !tools.is_empty() {
        body["tools"] = json!(tools);
        body["tool_choice"] = json!("auto");
    }
    if req.max_tokens > 0 {
        body["max_output_tokens"] = json!(req.max_tokens);
    }
    // Reasoning effort (H7a): `reasoning.effort`, merged into any existing
    // `reasoning` object; absent when unset. The API has three levels, so
    // `xhigh` and `max` map to `high`.
    if let Some(effort) = req.effort {
        if !body["reasoning"].is_object() {
            body["reasoning"] = json!({});
        }
        body["reasoning"]["effort"] = json!(match effort {
            modron_core::Effort::Low => "low",
            modron_core::Effort::Medium => "medium",
            modron_core::Effort::High | modron_core::Effort::Xhigh | modron_core::Effort::Max => {
                "high"
            }
        });
    }
    body
}

/// Incremental state for one streaming Responses turn.
struct ResponsesAccumulator {
    /// Provider id stamped on reasoning blocks and error messages.
    provider: &'static str,
    text: String,
    tools: Vec<ToolCall>,
    usage: Option<Usage>,
    /// Reasoning output items, kept verbatim for replay (H7b-2), keyed by
    /// their output position.
    reasoning: Vec<ProviderBlock>,
    done: bool,
}

impl Default for ResponsesAccumulator {
    fn default() -> Self {
        Self::new(PROVIDER_ID)
    }
}

impl ResponsesAccumulator {
    fn new(provider: &'static str) -> Self {
        Self {
            provider,
            text: String::new(),
            tools: Vec::new(),
            usage: None,
            reasoning: Vec::new(),
            done: false,
        }
    }

    fn ingest(
        &mut self,
        payload: &str,
        tx: &UnboundedSender<modron_core::StreamDelta>,
    ) -> Result<()> {
        let event: Value = serde_json::from_str(payload).map_err(|err| Error::Provider {
            provider: self.provider.into(),
            message: format!("malformed stream event: {err}"),
        })?;
        match event.get("type").and_then(Value::as_str).unwrap_or("") {
            "response.output_text.delta" => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    if !delta.is_empty() {
                        self.text.push_str(delta);
                        let _ = tx.send(modron_core::StreamDelta::Text(delta.to_string()));
                    }
                }
            }
            "response.reasoning_summary_text.delta" => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    if !delta.is_empty() {
                        let _ = tx.send(modron_core::StreamDelta::Reasoning(delta.to_string()));
                    }
                }
            }
            "response.output_item.done" => {
                if let Some(item) = event.get("item") {
                    if item.get("type").and_then(Value::as_str) == Some("reasoning") {
                        let index = event
                            .get("output_index")
                            .and_then(Value::as_u64)
                            .unwrap_or(self.reasoning.len() as u64);
                        self.reasoning.push(ProviderBlock {
                            provider: self.provider.into(),
                            api: REPLAY_API.into(),
                            kind: "reasoning".into(),
                            index: u32::try_from(index).unwrap_or(u32::MAX),
                            payload: item.clone(),
                        });
                    }
                    if item.get("type").and_then(Value::as_str) == Some("function_call") {
                        let name = item
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        // Empty arguments are a legal no-arg call; anything
                        // else must parse — swallowing malformed JSON into
                        // null would execute the call with the wrong shape.
                        let raw_args = item
                            .get("arguments")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let arguments = if raw_args.trim().is_empty() {
                            json!({})
                        } else {
                            serde_json::from_str(raw_args).map_err(|err| Error::Provider {
                                provider: self.provider.into(),
                                message: format!("malformed tool arguments for `{name}`: {err}"),
                            })?
                        };
                        let call = ToolCall {
                            id: item
                                .get("call_id")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            name,
                            arguments,
                        };
                        self.tools.push(call.clone());
                        // Complete item mid-stream: the engine may start
                        // speculating on it while later items stream.
                        let _ = tx.send(modron_core::StreamDelta::ToolCallReady(call));
                    }
                }
            }
            "response.completed" => {
                if let Some(usage) = event.pointer("/response/usage") {
                    self.usage = Some(Usage {
                        input_tokens: usage
                            .get("input_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                        output_tokens: usage
                            .get("output_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                        cached_input_tokens: usage
                            .pointer("/input_tokens_details/cached_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                    });
                }
            }
            "response.failed" | "error" => {
                let message = event
                    .pointer("/response/error/message")
                    .or_else(|| event.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown backend error");
                return Err(Error::Provider {
                    provider: self.provider.into(),
                    message: message.to_string(),
                });
            }
            _ => {}
        }
        Ok(())
    }

    fn finish(&self) -> Result<TurnResponse> {
        if !self.done && self.usage.is_none() {
            return Err(Error::Provider {
                provider: self.provider.into(),
                message: "connection error: stream ended before completion".into(),
            });
        }
        let stop_reason = if self.tools.is_empty() {
            StopReason::EndTurn
        } else {
            StopReason::ToolUse
        };
        let mut provider_blocks = self.reasoning.clone();
        provider_blocks.sort_by_key(|block| block.index);
        // Stamped by `ingest` with `self.provider`; a block from one mode never
        // matches the other mode's `replayable_blocks` filter.
        Ok(TurnResponse {
            text: self.text.clone(),
            tool_calls: self.tools.clone(),
            usage: self.usage.unwrap_or_default(),
            provider_blocks,
            stop_reason,
        })
    }
}

/// What a `refresh_token` grant returned. Nothing is persisted here.
pub(crate) struct RefreshGrant {
    pub access_token: String,
    /// The rotated refresh token, or `None` when the response omits it.
    pub refresh_token: Option<String>,
    /// chatgpt-account-id from the response's id_token, when present.
    pub account_id: Option<String>,
}

/// POST the `refresh_token` grant to `token_url`. Endpoint-parameterized
/// so tests can point it at a local mock. Errors never echo token values
/// beyond the HTTP status and the server's error text.
pub(crate) async fn refresh_grant(
    http: &Client,
    token_url: &str,
    refresh_token: &str,
) -> Result<RefreshGrant> {
    let response = http
        .post(token_url)
        .timeout(REQUEST_TIMEOUT)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", chatgpt::CLIENT_ID),
            ("refresh_token", refresh_token),
        ])
        .send()
        .await
        .map_err(|err| crate::transport_error(PROVIDER_ID, err))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|err| crate::transport_error(PROVIDER_ID, err))?;
    if !status.is_success() {
        return Err(Error::Provider {
            provider: PROVIDER_ID.into(),
            message: format!("token refresh failed: HTTP {status}: {text}"),
        });
    }
    let value: Value = serde_json::from_str(&text).map_err(|err| Error::Provider {
        provider: PROVIDER_ID.into(),
        message: format!("malformed token refresh response: {err}"),
    })?;
    let access_token = value
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Provider {
            provider: PROVIDER_ID.into(),
            message: "token refresh response missing access_token".into(),
        })?
        .to_string();
    Ok(RefreshGrant {
        access_token,
        refresh_token: value
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(String::from),
        account_id: value
            .get("id_token")
            .and_then(Value::as_str)
            .and_then(chatgpt::account_id),
    })
}

#[cfg(test)]
mod tests {
    use modron_core::auth::oauth::base64url_nopad;

    /// Reads one HTTP request off the socket and returns its body.
    fn read_request_body(sock: &mut std::net::TcpStream) -> String {
        use std::io::Read;
        let mut raw = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            match sock.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => raw.extend_from_slice(&chunk[..n]),
            }
            let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
                continue;
            };
            let head = String::from_utf8_lossy(&raw[..pos]).to_ascii_lowercase();
            let len = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if raw.len() >= pos + 4 + len {
                break;
            }
        }
        let start = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map_or(raw.len(), |pos| pos + 4);
        String::from_utf8_lossy(&raw[start..]).into_owned()
    }

    /// One-shot mock token endpoint: answers the next request with
    /// `status` and `body`, and sends back the request body it received.
    fn mock_token_endpoint(
        status: &'static str,
        body: impl Into<String>,
    ) -> (String, std::sync::mpsc::Receiver<String>) {
        use std::io::Write;
        let body: String = body.into();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let _ = tx.send(read_request_body(&mut sock));
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes());
            }
        });
        (format!("http://{addr}/oauth/token"), rx)
    }

    #[tokio::test]
    async fn refresh_grant_posts_the_refresh_grant_and_reads_rotated_tokens() {
        let jwt = format!(
            "h.{}.s",
            base64url_nopad(br#"{"chatgpt_account_id":"acc-9"}"#)
        );
        let body =
            format!(r#"{{"access_token":"at-new","refresh_token":"rt-new","id_token":"{jwt}"}}"#);
        let (url, request) = mock_token_endpoint("200 OK", body);
        let grant = refresh_grant(&Client::new(), &url, "rt-old").await.unwrap();
        assert_eq!(grant.access_token, "at-new");
        assert_eq!(grant.refresh_token.as_deref(), Some("rt-new"));
        assert_eq!(grant.account_id.as_deref(), Some("acc-9"));
        let sent = request.recv().unwrap();
        assert!(sent.contains("grant_type=refresh_token"), "{sent}");
        assert!(sent.contains("refresh_token=rt-old"), "{sent}");
        assert!(
            sent.contains(&format!("client_id={}", chatgpt::CLIENT_ID)),
            "{sent}"
        );
    }

    #[tokio::test]
    async fn refresh_grant_without_rotation_or_id_token_leaves_them_none() {
        let (url, _request) = mock_token_endpoint("200 OK", r#"{"access_token":"at-only"}"#);
        let grant = refresh_grant(&Client::new(), &url, "rt-old").await.unwrap();
        assert_eq!(grant.access_token, "at-only");
        assert_eq!(grant.refresh_token, None, "caller keeps the previous token");
        assert_eq!(grant.account_id, None, "caller keeps the stored account");
    }

    #[tokio::test]
    async fn refresh_grant_reports_rejection_and_missing_access_token() {
        let (url, _request) =
            mock_token_endpoint("401 Unauthorized", r#"{"error":"invalid_grant"}"#);
        let err = refresh_grant(&Client::new(), &url, "rt-dead")
            .await
            .err()
            .expect("rejected refresh must fail");
        assert!(err.to_string().contains("HTTP 401"), "{err}");

        let (url, _request) = mock_token_endpoint("200 OK", r#"{"refresh_token":"rt"}"#);
        let err = refresh_grant(&Client::new(), &url, "rt-old")
            .await
            .err()
            .expect("missing access_token must fail");
        assert!(err.to_string().contains("missing access_token"), "{err}");
    }

    use super::*;
    use modron_core::{Message, ToolSpec};

    fn request() -> TurnRequest {
        TurnRequest {
            model: "gpt-5".into(),
            system: "be brief".into(),
            messages: vec![
                Message::user("list the files"),
                Message {
                    role: Role::Assistant,
                    content: "checking".into(),
                    tool_calls: vec![ToolCall {
                        id: "call-1".into(),
                        name: "bash".into(),
                        arguments: json!({"command": "ls"}),
                    }],
                    tool_call_id: None,
                    attachments: Vec::new(),
                    provider_blocks: Vec::new(),
                },
                Message {
                    role: Role::Tool,
                    content: "a.txt b.txt".into(),
                    tool_calls: Vec::new(),
                    tool_call_id: Some("call-1".into()),
                    attachments: Vec::new(),
                    provider_blocks: Vec::new(),
                },
            ],
            tools: vec![ToolSpec {
                name: "bash".into(),
                description: "run a shell command".into(),
                parameters: json!({"type": "object"}),
            }],
            max_tokens: 1024,
            effort: None,
            reasoning_summary: false,
        }
    }

    #[test]
    fn effort_is_absent_from_the_body_when_unset() {
        let body = build_body(&request());
        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn effort_maps_to_reasoning_effort_with_xhigh_and_max_as_high() {
        use modron_core::Effort;
        for (effort, word) in [
            (Effort::Low, "low"),
            (Effort::Medium, "medium"),
            (Effort::High, "high"),
            (Effort::Xhigh, "high"),
            (Effort::Max, "high"),
        ] {
            let mut req = request();
            req.effort = Some(effort);
            let body = build_body(&req);
            assert_eq!(body["reasoning"], json!({ "effort": word }), "{effort:?}");
            assert_eq!(body["store"], false, "rest of the body unchanged");
        }
    }

    #[test]
    fn reasoning_items_round_trip_before_the_call_they_preceded() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut acc = ResponsesAccumulator::default();
        let reasoning_item = json!({
            "type": "reasoning",
            "id": "rs_0abc",
            "summary": [{"type": "summary_text", "text": "list first"}],
            "encrypted_content": "gAAAAABencrypted+/=="
        });
        // Output positions out of arrival order: the reasoning item is 0.
        acc.ingest(
            &json!({
                "type": "response.output_item.done",
                "output_index": 1,
                "item": {"type": "function_call", "call_id": "c1", "name": "bash", "arguments": "{}"}
            })
            .to_string(),
            &tx,
        )
        .unwrap();
        acc.ingest(
            &json!({"type": "response.output_item.done", "output_index": 0, "item": reasoning_item})
                .to_string(),
            &tx,
        )
        .unwrap();
        acc.done = true;
        let parsed = acc.finish().unwrap();
        assert_eq!(parsed.provider_blocks.len(), 1);
        assert_eq!(parsed.provider_blocks[0].payload, reasoning_item);

        let mut req = request();
        req.effort = Some(modron_core::Effort::High);
        req.messages = vec![
            Message::user("hi"),
            Message {
                role: Role::Assistant,
                content: String::new(),
                tool_calls: parsed.tool_calls.clone(),
                tool_call_id: None,
                attachments: Vec::new(),
                provider_blocks: parsed.provider_blocks.clone(),
            },
        ];
        let body = build_body(&req);
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[1], reasoning_item, "byte-for-byte item");
        assert_eq!(input[2]["type"], "function_call");

        // Off: no include key at all.
        req.effort = None;
        assert!(build_body(&req).get("include").is_none());
    }

    #[test]
    fn reasoning_items_from_another_provider_are_not_replayed() {
        let mut req = request();
        req.messages = vec![Message {
            role: Role::Assistant,
            content: "ok".into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            attachments: Vec::new(),
            provider_blocks: vec![ProviderBlock {
                provider: "anthropic".into(),
                api: "messages".into(),
                kind: "thinking".into(),
                index: 0,
                payload: json!({"type": "thinking", "thinking": "t", "signature": "s"}),
            }],
        }];
        let body = build_body(&req);
        assert_eq!(body["input"].as_array().unwrap().len(), 1);
        assert_eq!(body["input"][0]["type"], "message");
    }

    #[test]
    fn body_maps_the_transcript_to_typed_input_items() {
        let body = build_body(&request());
        assert_eq!(body["model"], "gpt-5");
        assert_eq!(body["instructions"], "be brief");
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[1]["content"][0]["type"], "output_text");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["call_id"], "call-1");
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["output"], "a.txt b.txt");
        assert_eq!(body["tools"][0]["name"], "bash");
        assert_eq!(body["max_output_tokens"], 1024);
    }

    #[test]
    fn events_decode_text_tools_and_usage() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut acc = ResponsesAccumulator::default();
        let events = [
            r#"{"type":"response.output_text.delta","delta":"hello "}"#,
            r#"{"type":"response.output_text.delta","delta":"world"}"#,
            r#"{"type":"response.reasoning_summary_text.delta","delta":"thinking"}"#,
            r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"c1","name":"bash","arguments":"{\"command\":\"ls\"}"}}"#,
            r#"{"type":"response.completed","response":{"usage":{"input_tokens":100,"output_tokens":7}}}"#,
        ];
        for e in events {
            acc.ingest(e, &tx).unwrap();
        }
        acc.done = true;
        let parsed = acc.finish().unwrap();
        assert_eq!(parsed.text, "hello world");
        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].name, "bash");
        assert_eq!(parsed.tool_calls[0].arguments["command"], "ls");
        assert_eq!(parsed.usage.input_tokens, 100);
        assert_eq!(parsed.stop_reason, StopReason::ToolUse);
        // Deltas streamed: two text + one reasoning.
        let mut text = 0;
        let mut reasoning = 0;
        while let Ok(d) = rx.try_recv() {
            match d {
                modron_core::StreamDelta::Text(_) => text += 1,
                modron_core::StreamDelta::Reasoning(_) => reasoning += 1,
                modron_core::StreamDelta::ToolCallReady(_) => {}
                modron_core::StreamDelta::Done => {}
            }
        }
        assert_eq!((text, reasoning), (2, 1));
    }

    #[test]
    fn failed_events_surface_the_backend_message() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut acc = ResponsesAccumulator::default();
        let err = acc
            .ingest(
                r#"{"type":"response.failed","response":{"error":{"message":"usage limit"}}}"#,
                &tx,
            )
            .unwrap_err();
        assert!(err.to_string().contains("usage limit"), "{err}");
    }

    #[test]
    fn body_encodes_attachments_as_input_parts() {
        let mut req = request();
        // Replace the transcript with ONE user message carrying both
        // attachment kinds.
        req.messages = vec![Message {
            role: Role::User,
            content: "see this".into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            attachments: vec![
                modron_core::Attachment::Image {
                    media_type: "image/png".into(),
                    data: "QUJD".into(),
                },
                modron_core::Attachment::Document {
                    name: "notes.txt".into(),
                    text: "hello doc".into(),
                },
            ],
            provider_blocks: Vec::new(),
        }];
        let body = build_body(&req);
        let content = &body["input"][0]["content"];
        assert_eq!(content.as_array().unwrap().len(), 3);
        assert_eq!(content[1]["type"], "input_image");
        assert_eq!(content[1]["image_url"], "data:image/png;base64,QUJD");
        assert_eq!(content[2]["type"], "input_text");
        assert!(content[2]["text"]
            .as_str()
            .unwrap()
            .contains("[Attached document: notes.txt]"));
    }

    #[test]
    fn provider_builds_from_explicit_credentials() {
        let provider = OpenAiResponsesProvider::new("tok", "acc").unwrap();
        assert_eq!(provider.id(), "chatgpt");
        assert_eq!(provider.base_url(), chatgpt::BACKEND_URL);
    }

    fn reasoning_item() -> Value {
        json!({
            "type": "reasoning",
            "id": "rs_0abc",
            "summary": [],
            "encrypted_content": "gAAAAABencrypted+/=="
        })
    }

    fn assistant_with_block(provider: &str, payload: Value) -> Message {
        Message {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments: json!({"command": "ls"}),
            }],
            tool_call_id: None,
            attachments: Vec::new(),
            provider_blocks: vec![ProviderBlock {
                provider: provider.into(),
                api: REPLAY_API.into(),
                kind: "reasoning".into(),
                index: 0,
                payload,
            }],
        }
    }

    #[test]
    fn api_key_provider_takes_the_v1_base_and_appends_responses() {
        let provider =
            OpenAiResponsesProvider::with_api_key("sk-test", "http://127.0.0.1:18090/v1/").unwrap();
        assert_eq!(provider.id(), API_KEY_PROVIDER_ID);
        assert_eq!(provider.base_url(), "http://127.0.0.1:18090/v1");
        assert_eq!(provider.endpoint(), "http://127.0.0.1:18090/v1/responses");
        let default =
            OpenAiResponsesProvider::with_api_key("sk-test", API_KEY_DEFAULT_BASE_URL).unwrap();
        assert_eq!(default.endpoint(), "https://api.openai.com/v1/responses");
        // The ChatGPT endpoint is untouched: BACKEND_URL is already complete.
        let chatgpt = OpenAiResponsesProvider::new("tok", "acc").unwrap();
        assert_eq!(chatgpt.endpoint(), chatgpt::BACKEND_URL);
    }

    #[test]
    fn api_key_body_matches_the_chatgpt_body_shape() {
        let mut req = request();
        // Without reasoning: byte-identical bodies.
        assert_eq!(build_body_for(&req, API_KEY_PROVIDER_ID), build_body(&req));
        // With reasoning: same include + reasoning.effort in both modes.
        req.effort = Some(modron_core::Effort::Low);
        let body = build_body_for(&req, API_KEY_PROVIDER_ID);
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(body["reasoning"], json!({"effort": "low"}));
        assert_eq!(body, build_body(&req));
        // Off again: no include, no reasoning key.
        req.effort = None;
        let body = build_body_for(&req, API_KEY_PROVIDER_ID);
        assert!(body.get("include").is_none());
        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn blocks_replay_only_to_the_provider_that_produced_them() {
        let item = reasoning_item();
        // An API-key block replays to the API-key provider...
        let mut req = request();
        req.messages = vec![assistant_with_block(API_KEY_PROVIDER_ID, item.clone())];
        let api = build_body_for(&req, API_KEY_PROVIDER_ID);
        assert_eq!(api["input"][0], item, "own block replays first");
        assert_eq!(api["input"][1]["type"], "function_call");
        // ...but not to the ChatGPT provider, and a ChatGPT block never
        // replays to the API-key provider (separate account scope).
        let chatgpt_body = build_body_for(&req, PROVIDER_ID);
        assert_eq!(chatgpt_body["input"][0]["type"], "function_call");
        req.messages = vec![assistant_with_block(PROVIDER_ID, item.clone())];
        let api = build_body_for(&req, API_KEY_PROVIDER_ID);
        assert_eq!(api["input"][0]["type"], "function_call");
        let chatgpt_body = build_body_for(&req, PROVIDER_ID);
        assert_eq!(chatgpt_body["input"][0], item);
    }

    #[test]
    fn accumulator_stamps_blocks_with_its_own_provider_id() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut acc = ResponsesAccumulator::new(API_KEY_PROVIDER_ID);
        acc.ingest(
            &json!({"type": "response.output_item.done", "output_index": 0, "item": reasoning_item()})
                .to_string(),
            &tx,
        )
        .unwrap();
        acc.done = true;
        let parsed = acc.finish().unwrap();
        assert_eq!(parsed.provider_blocks[0].provider, API_KEY_PROVIDER_ID);
        assert_eq!(parsed.provider_blocks[0].api, REPLAY_API);
        // Default stays the ChatGPT id.
        let default = ResponsesAccumulator::default();
        assert_eq!(default.provider, PROVIDER_ID);
    }

    /// One-shot local server: captures the request text, answers with a
    /// canned Responses SSE stream (reasoning item, text, usage).
    async fn scripted_responses_server() -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 1 << 20];
            let n = socket.read(&mut buf).await.unwrap();
            let sse = [
                json!({"type": "response.output_item.done", "output_index": 0, "item": reasoning_item()}),
                json!({"type": "response.output_text.delta", "delta": "hi"}),
                json!({"type": "response.completed", "response": {"usage": {"input_tokens": 9, "output_tokens": 2, "input_tokens_details": {"cached_tokens": 0}}}}),
            ]
            .iter()
            .map(|e| format!("data: {e}\n\n"))
            .collect::<String>()
                + "data: [DONE]\n\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                sse.len(),
                sse
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&buf[..n]).to_string()
        });
        (format!("http://{addr}/v1"), handle)
    }

    #[tokio::test]
    async fn api_key_request_sends_bearer_only_and_parses_the_stream() {
        let (base, server) = scripted_responses_server().await;
        let provider = OpenAiResponsesProvider::with_api_key("sk-test-key", base).unwrap();
        let mut req = request();
        req.effort = Some(modron_core::Effort::Low);
        let turn = provider.turn(std::sync::Arc::new(req)).await.unwrap();
        assert_eq!(turn.text, "hi");
        assert_eq!(turn.provider_blocks.len(), 1);
        assert_eq!(turn.provider_blocks[0].provider, API_KEY_PROVIDER_ID);

        let raw = server.await.unwrap().to_ascii_lowercase();
        assert!(raw.starts_with("post /v1/responses "), "{raw}");
        assert!(raw.contains("authorization: bearer sk-test-key"), "{raw}");
        for backend_only in ["chatgpt-account-id", "originator:", "openai-beta"] {
            assert!(
                !raw.contains(backend_only),
                "{backend_only} must not be sent"
            );
        }
        assert!(raw.contains("\"include\":[\"reasoning.encrypted_content\"]"));
        assert!(raw.contains("\"store\":false"));
    }
}
