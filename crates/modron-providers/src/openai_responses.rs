//! OpenAI Responses provider over the ChatGPT backend (subscription auth).
//!
//! ChatGPT-plan traffic does NOT go to api.openai.com: the Codex-style
//! backend at `chatgpt.com/backend-api/codex/responses` speaks the
//! Responses API (`instructions` + typed `input` items) and requires the
//! `chatgpt-account-id` header alongside the OAuth bearer token. Login is
//! the device flow in `modron_core::auth::oauth::chatgpt` (`/auth add` in
//! the TUI); tokens live 0600 in `~/.modron/auth/openai.json`.
//!
//! Request building ([`build_body`]) and event decoding
//! ([`ResponsesAccumulator`]) are pure; tests assert wire shape and parse
//! scripted SSE fixtures — no network. Live verification needs a real
//! ChatGPT sign-in (the user's browser round-trip).

use crate::stream::parse_sse_stream;
use async_trait::async_trait;
use futures::StreamExt;
use modron_core::auth::oauth::chatgpt;
use modron_core::{
    Error, ModelProvider, Result, Role, StopReason, ToolCall, TurnRequest, TurnResponse, Usage,
};
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

/// ChatGPT (subscription) provider id ([`ModelProvider::id`]).
pub const PROVIDER_ID: &str = "chatgpt";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const DONE_SENTINEL: &str = "[DONE]";

/// A ChatGPT-subscription provider: Responses API on the Codex backend.
pub struct OpenAiResponsesProvider {
    access_token: String,
    account_id: String,
    base_url: String,
    http: Client,
}

impl OpenAiResponsesProvider {
    /// Build from explicitly passed credentials.
    pub fn new(access_token: impl Into<String>, account_id: impl Into<String>) -> Result<Self> {
        Ok(Self {
            access_token: access_token.into(),
            account_id: account_id.into(),
            base_url: chatgpt::BACKEND_URL.to_string(),
            http: build_client()?,
        })
    }

    /// Build from the stored `/auth add` sign-in. Actionable error when no
    /// sign-in exists.
    pub fn from_stored_tokens() -> Result<Self> {
        match chatgpt::load() {
            Ok(Some(auth)) => Self::new(auth.access_token, auth.account_id),
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
        req: TurnRequest,
        tx: UnboundedSender<modron_core::StreamDelta>,
    ) -> Result<TurnResponse> {
        let url = format!("{}/responses", self.base_url.trim_end_matches('/'));
        let body = build_body(&req);
        let response = self.post(&url, &body).await?;
        let status = response.status();
        if !status.is_success() {
            let text = response
                .text()
                .await
                .map_err(|err| crate::transport_error(PROVIDER_ID, err))?;
            return Err(crate::http_error(PROVIDER_ID, status, &text));
        }

        let mut buffer: Vec<u8> = Vec::new();
        let mut byte_stream = response.bytes_stream();
        let mut acc = ResponsesAccumulator::default();
        'read: while let Some(item) = byte_stream.next().await {
            let chunk = item.map_err(|err| crate::transport_error(PROVIDER_ID, err))?;
            buffer.extend_from_slice(&chunk);
            let (payloads, consumed) = parse_sse_stream(&buffer);
            buffer.drain(..consumed);
            for payload in payloads {
                if payload == DONE_SENTINEL {
                    acc.done = true;
                    break 'read;
                }
                acc.ingest(&payload, &tx)?;
            }
        }
        let parsed = acc.finish()?;
        let _ = tx.send(modron_core::StreamDelta::Done);
        Ok(parsed)
    }

    async fn post(&self, url: &str, body: &Value) -> Result<reqwest::Response> {
        self.http
            .post(url)
            .header("Authorization", format!("Bearer {}", self.access_token))
            .header("chatgpt-account-id", &self.account_id)
            .header("OpenAI-Beta", "responses=experimental")
            .header("originator", "modron")
            .json(body)
            .send()
            .await
            .map_err(|err| crate::transport_error(PROVIDER_ID, err))
    }
}

#[async_trait]
impl ModelProvider for OpenAiResponsesProvider {
    fn id(&self) -> &str {
        PROVIDER_ID
    }

    async fn turn(&self, req: TurnRequest) -> Result<TurnResponse> {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        self.turn_streaming_inner(req, tx).await
    }

    async fn turn_streaming(
        &self,
        req: TurnRequest,
        tx: UnboundedSender<modron_core::StreamDelta>,
    ) -> Result<TurnResponse> {
        self.turn_streaming_inner(req, tx).await
    }
}

fn build_client() -> Result<Client> {
    Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .map_err(|err| Error::Provider {
            provider: PROVIDER_ID.into(),
            message: format!("cannot build http client: {err}"),
        })
}

/// Map a [`TurnRequest`] to the Responses API body shape: `instructions`
/// carries the system prompt and the transcript becomes typed `input`
/// items (message / function_call / function_call_output).
pub fn build_body(req: &TurnRequest) -> Value {
    let mut input: Vec<Value> = Vec::new();
    for message in &req.messages {
        match message.role {
            Role::User => input.push(json!({
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": message.content}],
            })),
            Role::Assistant => {
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
    if !tools.is_empty() {
        body["tools"] = json!(tools);
        body["tool_choice"] = json!("auto");
    }
    if req.max_tokens > 0 {
        body["max_output_tokens"] = json!(req.max_tokens);
    }
    body
}

/// Incremental state for one streaming Responses turn.
#[derive(Default)]
struct ResponsesAccumulator {
    text: String,
    tools: Vec<ToolCall>,
    usage: Option<Usage>,
    done: bool,
}

impl ResponsesAccumulator {
    fn ingest(
        &mut self,
        payload: &str,
        tx: &UnboundedSender<modron_core::StreamDelta>,
    ) -> Result<()> {
        let event: Value = serde_json::from_str(payload).map_err(|err| Error::Provider {
            provider: PROVIDER_ID.into(),
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
                    if item.get("type").and_then(Value::as_str) == Some("function_call") {
                        self.tools.push(ToolCall {
                            id: item
                                .get("call_id")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            name: item
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            arguments: item
                                .get("arguments")
                                .and_then(Value::as_str)
                                .and_then(|a| serde_json::from_str(a).ok())
                                .unwrap_or(Value::Null),
                        });
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
                        cached_input_tokens: 0,
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
                    provider: PROVIDER_ID.into(),
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
                provider: PROVIDER_ID.into(),
                message: "connection error: stream ended before completion".into(),
            });
        }
        let stop_reason = if self.tools.is_empty() {
            StopReason::EndTurn
        } else {
            StopReason::ToolUse
        };
        Ok(TurnResponse {
            text: self.text.clone(),
            tool_calls: self.tools.clone(),
            usage: self.usage.unwrap_or_default(),
            stop_reason,
        })
    }
}

#[cfg(test)]
mod tests {
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
                },
                Message {
                    role: Role::Tool,
                    content: "a.txt b.txt".into(),
                    tool_calls: Vec::new(),
                    tool_call_id: Some("call-1".into()),
                    attachments: Vec::new(),
                },
            ],
            tools: vec![ToolSpec {
                name: "bash".into(),
                description: "run a shell command".into(),
                parameters: json!({"type": "object"}),
            }],
            max_tokens: 1024,
        }
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
    fn provider_builds_from_explicit_credentials() {
        let provider = OpenAiResponsesProvider::new("tok", "acc").unwrap();
        assert_eq!(provider.id(), "chatgpt");
        assert_eq!(provider.base_url(), chatgpt::BACKEND_URL);
    }
}
