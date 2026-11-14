//! OpenAI-compatible chat-completions provider — LEDGER item P2.
//!
//! Non-streaming `POST {base_url}/chat/completions` with the OpenAI
//! function-calling schema. The `base_url` must include the version path, so
//! the same client covers OpenAI (`https://api.openai.com/v1`), Ollama
//! (`http://localhost:11434/v1`), OpenRouter (`https://openrouter.ai/api/v1`),
//! DeepSeek, llama.cpp, vLLM, …
//!
//! Request building ([`build_body`]) and response parsing ([`parse_response`])
//! are pure functions; tests assert wire shape and parse recorded-JSON
//! fixtures — no network.

use crate::stream::StreamDelta;
use async_trait::async_trait;
use modron_core::{
    Error, ModelProvider, Result, Role, StopReason, ToolCall, TurnRequest, TurnResponse, Usage,
};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedSender;

/// OpenAI-compat provider id ([`ModelProvider::id`]).
pub const PROVIDER_ID: &str = "openai-compat";

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
use crate::http::{
    build_client, check_status, drain_sse, post_json, within_idle, STREAM_IDLE_TIMEOUT,
};

/// Any OpenAI-compatible chat-completions endpoint.
pub struct OpenAiCompatProvider {
    api_key: String,
    base_url: String,
    http: Client,
}

impl OpenAiCompatProvider {
    /// Create a client against `https://api.openai.com/v1`. An empty key is
    /// allowed (local servers ignore it); the `Authorization` header is then
    /// omitted entirely. Fails only if the HTTP client cannot be built.
    pub fn new(api_key: impl Into<String>) -> Result<Self> {
        Ok(Self {
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
            http: build_client()?,
        })
    }

    /// Point the client at another OpenAI-compatible deployment. The base URL
    /// must include the version path (e.g. `http://localhost:11434/v1`); the
    /// `/chat/completions` suffix is appended per request.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        if !self.api_key.is_empty() {
            crate::http::warn_cleartext_once(PROVIDER_ID, &self.base_url);
        }
        self
    }

    /// The Bearer header when a key is set; an empty key sends NO
    /// Authorization header at all (local servers ignore credentials).
    fn bearer_headers(&self) -> Vec<(&'static str, String)> {
        if self.api_key.is_empty() {
            Vec::new()
        } else {
            vec![("Authorization", format!("Bearer {}", self.api_key))]
        }
    }

    /// Configured base URL (for diagnostics and factory tests).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Streaming variant of [`ModelProvider::turn`] — LEDGER item S1
    /// (inherent method; deliberately NOT on the [`ModelProvider`] trait).
    ///
    /// POSTs the same `/chat/completions` body plus `"stream": true` and
    /// `"stream_options": {"include_usage": true}` (asking for the usage
    /// frame; endpoints that ignore it fall through to the same zeros +
    /// warning as the non-streaming path). Decodes the OpenAI chunk
    /// protocol: `choices[0].delta.content` → [`StreamDelta::Text`],
    /// `choices[0].delta.tool_calls[{index, id, function{name, arguments}}]`
    /// accumulated per tool index with `arguments` arriving in fragments,
    /// `finish_reason` on the final choice, then the usage-only chunk
    /// (`choices: []`), then the `data: [DONE]` sentinel which terminates.
    ///
    /// The returned [`TurnResponse`] must equal — field for field — what
    /// [`parse_response`] produces for the equivalent non-streaming body.
    /// `StreamDelta::Done` is sent after the last delta, only on success.
    ///
    /// `tx` is a best-effort observer: sends to a dropped receiver are
    /// ignored (the authoritative result is the returned [`TurnResponse`]).
    ///
    /// NOT auto-retried: a retried stream would re-emit deltas to `tx`.
    /// Transport failures use the same message-prefix taxonomy as
    /// [`crate::retry::is_retryable`] (`timeout: …`, `connection error: …`,
    /// `request failed: …`) so the policy can be decided above this layer.
    /// A stream that ends without `[DONE]` is a truncated response and fails
    /// with a `connection error: …` prefix rather than a partial result.
    async fn turn_streaming_inner(
        &self,
        req: &TurnRequest,
        tx: UnboundedSender<StreamDelta>,
    ) -> Result<TurnResponse> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let mut body = build_body(req);
        body["stream"] = json!(true);
        body["stream_options"] = json!({ "include_usage": true });
        tracing::debug!(
            provider = PROVIDER_ID,
            model = %req.model,
            url = %url,
            messages = req.messages.len(),
            body_bytes = body.to_string().len(),
            "sending streaming turn request"
        );

        let mut request = self.http.post(&url).json(&body);
        if !self.api_key.is_empty() {
            request = request.header("Authorization", format!("Bearer {}", self.api_key));
        }
        let response = within_idle(PROVIDER_ID, STREAM_IDLE_TIMEOUT, async {
            request
                .send()
                .await
                .map_err(|err| crate::transport_error(PROVIDER_ID, err))
        })
        .await?;
        // Streaming failures arrive as a plain (non-SSE) error body; the
        // shared status check maps them to the canonical error text.
        let response = check_status(PROVIDER_ID, response).await?;

        let mut acc = OaiStreamAccumulator::default();
        drain_sse(
            PROVIDER_ID,
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

/// OpenAI's stream terminator, passed through [`crate::stream::parse_sse_stream`] verbatim.
const DONE_SENTINEL: &str = "[DONE]";

/// Incremental state for one streaming turn. Reconstructs exactly what
/// [`parse_response`] would produce for the equivalent complete body.
#[derive(Default)]
struct OaiStreamAccumulator {
    text: String,
    /// One entry per tool-call index, in first-appearance order; `id` and
    /// `name` arrive on the first fragment, `arguments` in fragments.
    tools: Vec<(i64, String, String, String)>,
    finish_reason: Option<String>,
    usage: Option<OaiUsage>,
    done: bool,
}

impl OaiStreamAccumulator {
    fn ingest(&mut self, payload: &str, tx: &UnboundedSender<StreamDelta>) -> Result<()> {
        let chunk: Value = serde_json::from_str(payload).map_err(|err| Error::Provider {
            provider: PROVIDER_ID.into(),
            message: format!("malformed stream chunk: {err}"),
        })?;
        // Some servers report a mid-stream failure as a top-level error
        // object instead of closing with an HTTP error. Surface the server's
        // message. The `stream error:` prefix is deliberately not a retry
        // class: the server already decided this failure.
        if let Some(error) = chunk.get("error").filter(|error| error.is_object()) {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .map_or_else(|| error.to_string(), str::to_string);
            return Err(Error::Provider {
                provider: PROVIDER_ID.into(),
                message: format!("stream error: {}", crate::snippet(&message, 300)),
            });
        }
        if let Some(usage) = chunk.get("usage").filter(|usage| !usage.is_null()) {
            let parsed: OaiUsage =
                serde_json::from_value(usage.clone()).map_err(|err| Error::Provider {
                    provider: PROVIDER_ID.into(),
                    message: format!("malformed response body: {err}"),
                })?;
            self.usage = Some(parsed);
        }
        // Only choices[0] is consumed, matching parse_response; the usage
        // chunk carries an empty choices array.
        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
        else {
            return Ok(());
        };
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(reason.into());
        }
        let Some(delta) = choice.get("delta") else {
            return Ok(());
        };

        if let Some(content) = delta.get("content").and_then(Value::as_str) {
            if !content.is_empty() {
                self.text.push_str(content);
                let _ = tx.send(StreamDelta::Text(content.to_string()));
            }
        }

        if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in tool_calls {
                let index = call.get("index").and_then(Value::as_i64).unwrap_or(0);
                if self.tools.iter().all(|(existing, ..)| *existing != index) {
                    self.tools
                        .push((index, String::new(), String::new(), String::new()));
                }
                let Some((_, id, name, arguments)) = self
                    .tools
                    .iter_mut()
                    .find(|(existing, ..)| *existing == index)
                else {
                    continue;
                };
                if let Some(call_id) = call.get("id").and_then(Value::as_str) {
                    if !call_id.is_empty() {
                        *id = call_id.into();
                    }
                }
                if let Some(function) = call.get("function") {
                    if let Some(function_name) = function.get("name").and_then(Value::as_str) {
                        if !function_name.is_empty() {
                            *name = function_name.into();
                        }
                    }
                    if let Some(fragment) = function.get("arguments").and_then(Value::as_str) {
                        arguments.push_str(fragment);
                    }
                }
            }
        }
        Ok(())
    }

    fn finish(self) -> Result<TurnResponse> {
        if !self.done {
            return Err(Error::Provider {
                provider: PROVIDER_ID.into(),
                message: "connection error: stream ended before [DONE]".into(),
            });
        }

        let mut tools = self.tools;
        tools.sort_by_key(|(index, ..)| *index);
        let mut tool_calls = Vec::new();
        for (_, id, name, arguments) in &tools {
            let arguments = if arguments.is_empty() {
                json!({})
            } else {
                serde_json::from_str(arguments).map_err(|err| Error::Provider {
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

        let usage = match self.usage {
            Some(usage) => Usage {
                input_tokens: usage.prompt_tokens,
                output_tokens: usage.completion_tokens,
                cached_input_tokens: usage
                    .prompt_tokens_details
                    .and_then(|details| details.cached_tokens)
                    .unwrap_or(0),
            },
            None => {
                // Same honesty as the non-streaming path: some proxies omit
                // usage even when include_usage was requested.
                tracing::warn!(
                    provider = PROVIDER_ID,
                    "stream missing usage (include_usage ignored by endpoint); accounting under-counts"
                );
                Usage::default()
            }
        };

        Ok(TurnResponse {
            text: self.text,
            tool_calls,
            usage,
            stop_reason: map_stop_reason(self.finish_reason.as_deref()),
        })
    }
}

/// Build the `/chat/completions` request body from a [`TurnRequest`].
///
/// Mapping rules:
/// - `system` → a leading `{"role": "system"}` message (omitted when empty).
/// - `Role::User` → `{"role": "user"}`.
/// - `Role::Assistant` → `{"role": "assistant"}` with `tool_calls` as OpenAI
///   function calls; `arguments` are re-stringified (the API carries them as
///   JSON strings).
/// - `Role::Tool` → `{"role": "tool", "tool_call_id": …}`.
/// - each [`ToolSpec`](modron_core::ToolSpec) → `{"type": "function",
///   "function": {"name", "description", "parameters"}}`.
pub fn build_body(req: &TurnRequest) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    if !req.system.is_empty() {
        messages.push(json!({ "role": "system", "content": req.system }));
    }

    for message in &req.messages {
        match message.role {
            Role::User => {
                // Multimodal (Phase 7): attachments become content parts —
                // images as image_url data URIs, documents as text parts.
                if message.attachments.is_empty() {
                    messages.push(json!({ "role": "user", "content": message.content }));
                } else {
                    let mut parts: Vec<Value> = Vec::new();
                    for attachment in &message.attachments {
                        match attachment {
                            modron_core::Attachment::Image { media_type, data } => {
                                parts.push(json!({
                                    "type": "image_url",
                                    "image_url": {
                                        "url": format!("data:{media_type};base64,{data}")
                                    }
                                }));
                            }
                            modron_core::Attachment::Document { name, text } => {
                                parts.push(json!({
                                    "type": "text",
                                    "text": format!("[Attached document: {name}]\n{text}")
                                }));
                            }
                        }
                    }
                    if !message.content.is_empty() {
                        parts.push(json!({ "type": "text", "text": message.content }));
                    }
                    messages.push(json!({ "role": "user", "content": parts }));
                }
            }
            Role::Tool => {
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": message.tool_call_id.clone().unwrap_or_default(),
                    "content": message.content,
                }));
            }
            Role::Assistant => {
                let mut message_json = json!({ "role": "assistant", "content": message.content });
                if !message.tool_calls.is_empty() {
                    message_json["tool_calls"] = Value::Array(
                        message
                            .tool_calls
                            .iter()
                            .map(|call| {
                                json!({
                                    "id": call.id,
                                    "type": "function",
                                    "function": {
                                        "name": call.name,
                                        "arguments": call.arguments.to_string(),
                                    },
                                })
                            })
                            .collect(),
                    );
                }
                messages.push(message_json);
            }
        }
    }

    let mut body = json!({
        "model": req.model,
        "messages": messages,
        "max_tokens": req.max_tokens,
    });
    if !req.tools.is_empty() {
        body["tools"] = Value::Array(
            req.tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.parameters,
                        },
                    })
                })
                .collect(),
        );
    }
    body
}

/// Parse a `/chat/completions` response body into a [`TurnResponse`].
///
/// Tool-call `arguments` arrive as JSON strings and are parsed eagerly; a
/// malformed string is a provider error. Missing usage logs a warning and
/// reports zeros (some proxies omit it) — accounting never invents numbers.
pub fn parse_response(body: Value) -> Result<TurnResponse> {
    let resp: OaiResponse = serde_json::from_value(body).map_err(|err| Error::Provider {
        provider: PROVIDER_ID.into(),
        message: format!("malformed response body: {err}"),
    })?;

    let choice = resp
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| Error::Provider {
            provider: PROVIDER_ID.into(),
            message: "response contained no choices".into(),
        })?;

    let mut tool_calls = Vec::new();
    for call in choice.message.tool_calls.unwrap_or_default() {
        let arguments = if call.function.arguments.is_empty() {
            json!({})
        } else {
            serde_json::from_str(&call.function.arguments).map_err(|err| Error::Provider {
                provider: PROVIDER_ID.into(),
                message: format!("malformed tool arguments from model: {err}"),
            })?
        };
        tool_calls.push(ToolCall {
            id: call.id,
            name: call.function.name,
            arguments,
        });
    }

    let usage = match resp.usage {
        Some(usage) => Usage {
            input_tokens: usage.prompt_tokens,
            output_tokens: usage.completion_tokens,
            cached_input_tokens: usage
                .prompt_tokens_details
                .and_then(|details| details.cached_tokens)
                .unwrap_or(0),
        },
        None => {
            tracing::warn!(
                provider = PROVIDER_ID,
                "response missing usage; accounting under-counts"
            );
            Usage::default()
        }
    };

    Ok(TurnResponse {
        text: choice.message.content.unwrap_or_default(),
        tool_calls,
        usage,
        stop_reason: map_stop_reason(choice.finish_reason.as_deref()),
    })
}

fn map_stop_reason(reason: Option<&str>) -> StopReason {
    match reason {
        Some("stop") => StopReason::EndTurn,
        Some("tool_calls" | "function_call") => StopReason::ToolUse,
        Some("length") => StopReason::MaxTokens,
        _ => StopReason::Other,
    }
}

#[derive(Debug, Deserialize)]
struct OaiResponse {
    choices: Vec<OaiChoice>,
    #[serde(default)]
    usage: Option<OaiUsage>,
}

#[derive(Debug, Deserialize)]
struct OaiChoice {
    message: OaiMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OaiMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OaiToolCall>>,
}

#[derive(Debug, Deserialize)]
struct OaiToolCall {
    #[serde(default)]
    id: String,
    #[serde(default)]
    function: OaiFunction,
}

#[derive(Debug, Default, Deserialize)]
struct OaiFunction {
    #[serde(default)]
    name: String,
    #[serde(default)]
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct OaiUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<OaiPromptDetails>,
}

#[derive(Debug, Deserialize)]
struct OaiPromptDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
}

#[async_trait]
impl ModelProvider for OpenAiCompatProvider {
    /// Trait-level streaming override (S1 lift): delegates to the inherent
    /// implementation so `dyn ModelProvider` dispatch streams, not just
    /// concrete-type callers.
    async fn turn_streaming(
        &self,
        req: std::sync::Arc<TurnRequest>,
        tx: tokio::sync::mpsc::UnboundedSender<StreamDelta>,
    ) -> Result<TurnResponse> {
        self.turn_streaming_inner(&req, tx).await
    }
    fn id(&self) -> &str {
        PROVIDER_ID
    }

    async fn turn(&self, req: std::sync::Arc<TurnRequest>) -> Result<TurnResponse> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let body = build_body(&req);
        tracing::debug!(
            provider = PROVIDER_ID,
            model = %req.model,
            url = %url,
            messages = req.messages.len(),
            body_bytes = body.to_string().len(),
            "sending turn request"
        );

        // POST + status-check + parse is the shared [`post_json`]; the
        // Bearer header rides only when a key is set (2026-11-12 audit
        // DRY fix — the inline scaffold was a verbatim twin of the
        // responses provider's).
        let headers = self.bearer_headers();
        let value = post_json(&self.http, PROVIDER_ID, &url, &headers, &body).await?;
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

    fn sample_request() -> TurnRequest {
        TurnRequest {
            model: "gpt-5-mini".into(),
            system: "You are Modron.".into(),
            messages: vec![
                Message::user("list the rust files"),
                {
                    let mut message = Message::assistant("Checking.");
                    message.tool_calls = vec![ToolCall {
                        id: "call_1".into(),
                        name: "glob".into(),
                        arguments: json!({"pattern": "*.rs"}),
                    }];
                    message
                },
                Message::tool_result("call_1", &ToolOutcome::ok("main.rs\nlib.rs")),
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
    fn attachments_encode_as_image_url_parts() {
        let req = TurnRequest {
            model: "glm-5.3".into(),
            system: String::new(),
            messages: vec![modron_core::Message::user_with_attachments(
                "describe",
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
        assert!(content.is_array(), "multimodal content is a parts array");
        assert_eq!(content[0]["type"], "image_url");
        assert_eq!(
            content[0]["image_url"]["url"],
            "data:image/png;base64,aGVsbG8="
        );
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "describe");
    }

    #[test]
    fn no_attachments_stay_plain_strings() {
        let req = TurnRequest {
            model: "glm-5.3".into(),
            system: String::new(),
            messages: vec![modron_core::Message::user("plain")],
            tools: Vec::new(),
            max_tokens: 128,
        };
        let body = build_body(&req);
        assert_eq!(body["messages"][0]["content"], "plain");
    }

    #[test]
    fn request_body_shape() {
        let body = build_body(&sample_request());
        assert_eq!(body["model"], "gpt-5-mini");
        assert_eq!(body["max_tokens"], 8192);

        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "You are Modron.");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "list the rust files");

        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[2]["content"], "Checking.");
        assert_eq!(messages[2]["tool_calls"][0]["id"], "call_1");
        assert_eq!(messages[2]["tool_calls"][0]["type"], "function");
        assert_eq!(messages[2]["tool_calls"][0]["function"]["name"], "glob");
        // Arguments are carried as a JSON string on the wire.
        assert_eq!(
            messages[2]["tool_calls"][0]["function"]["arguments"],
            r#"{"pattern":"*.rs"}"#
        );

        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "call_1");
        assert_eq!(messages[3]["content"], "main.rs\nlib.rs");

        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "glob");
        assert_eq!(tools[0]["function"]["parameters"]["required"][0], "pattern");
    }

    #[test]
    fn empty_system_message_is_omitted() {
        let req = TurnRequest {
            model: "m".into(),
            system: String::new(),
            messages: vec![Message::user("hi")],
            tools: vec![],
            max_tokens: 16,
        };
        let body = build_body(&req);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn body_never_carries_cache_control() {
        // K1 guard: OpenAI-compatible endpoints cache prompts automatically
        // (ordering discipline only); Anthropic-style cache_control
        // breakpoints belong to anthropic.rs's build_body alone.
        let body = build_body(&sample_request());
        assert!(
            !body.to_string().contains("cache_control"),
            "openai-compat body must stay free of cache_control: {body}"
        );
    }

    #[test]
    fn parses_recorded_fixture_with_tool_calls() {
        let fixture = r#"{
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "created": 1760000000,
            "model": "gpt-5-mini",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_9",
                        "type": "function",
                        "function": {"name": "read", "arguments": "{\"path\": \"src/lib.rs\"}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {
                "prompt_tokens": 1500,
                "completion_tokens": 88,
                "prompt_tokens_details": {"cached_tokens": 640}
            }
        }"#;
        let parsed = parse_response(serde_json::from_str::<Value>(fixture).unwrap()).unwrap();

        assert_eq!(parsed.text, "");
        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].id, "call_9");
        assert_eq!(parsed.tool_calls[0].name, "read");
        // Arguments are parsed eagerly into a JSON value.
        assert_eq!(parsed.tool_calls[0].arguments["path"], "src/lib.rs");
        assert_eq!(parsed.stop_reason, StopReason::ToolUse);
        assert_eq!(parsed.usage.input_tokens, 1500);
        assert_eq!(parsed.usage.output_tokens, 88);
        assert_eq!(parsed.usage.cached_input_tokens, 640);
    }

    #[test]
    fn parses_text_only_fixture() {
        let fixture = r#"{
            "choices": [{
                "message": {"role": "assistant", "content": "Hello there."},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 12, "completion_tokens": 4}
        }"#;
        let parsed = parse_response(serde_json::from_str::<Value>(fixture).unwrap()).unwrap();
        assert_eq!(parsed.text, "Hello there.");
        assert!(parsed.tool_calls.is_empty());
        assert_eq!(parsed.stop_reason, StopReason::EndTurn);
        assert_eq!(parsed.usage.input_tokens, 12);
        assert_eq!(parsed.usage.output_tokens, 4);
        assert_eq!(parsed.usage.cached_input_tokens, 0);
    }

    #[test]
    fn length_finish_reason_maps_to_max_tokens() {
        let fixture = r#"{
            "choices": [{"message": {"role": "assistant", "content": "cut off"}, "finish_reason": "length"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        }"#;
        let parsed = parse_response(serde_json::from_str::<Value>(fixture).unwrap()).unwrap();
        assert_eq!(parsed.stop_reason, StopReason::MaxTokens);
    }

    #[test]
    fn malformed_tool_arguments_are_provider_errors() {
        let fixture = r#"{
            "choices": [{
                "message": {
                    "role": "assistant", "content": null,
                    "tool_calls": [{"id": "c1", "function": {"name": "read", "arguments": "not json"}}]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        }"#;
        let parsed = parse_response(serde_json::from_str::<Value>(fixture).unwrap());
        assert!(
            matches!(parsed, Err(Error::Provider { .. })),
            "got: {parsed:?}"
        );
    }

    #[test]
    fn empty_choices_is_a_provider_error() {
        let parsed = parse_response(json!({"choices": []}));
        assert!(
            matches!(parsed, Err(Error::Provider { .. })),
            "got: {parsed:?}"
        );
    }

    #[test]
    fn missing_usage_reports_zero() {
        let fixture = r#"{
            "choices": [{"message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}]
        }"#;
        let parsed = parse_response(serde_json::from_str::<Value>(fixture).unwrap()).unwrap();
        assert_eq!(parsed.usage, Usage::default());
    }

    #[test]
    fn base_url_defaults_and_overrides() {
        let provider = OpenAiCompatProvider::new("sk-test").unwrap();
        assert_eq!(provider.base_url(), "https://api.openai.com/v1");
        // Ollama-style override: base URL carries the version path.
        let overridden = provider.with_base_url("http://localhost:11434/v1");
        assert_eq!(overridden.base_url(), "http://localhost:11434/v1");
    }
}
