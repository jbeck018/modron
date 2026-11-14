//! LEDGER item S1 — streaming wire-format proof (offline).
//!
//! The provider-layer streaming surface had never touched a wire, so this
//! file is the deliverable: a scripted local HTTP/1.1 server (raw
//! `TcpListener`, hand-written response, `Connection: close` — a
//! close-delimited SSE body, no chunked encoding; see `tests/common/mod.rs`)
//! serving realistic fixtures for both wire formats. Each test proves:
//!
//! 1. the exact request shape on the wire (`"stream": true`, OpenAI
//!    `"stream_options": {"include_usage": true}`, headers, path);
//! 2. deltas arrive in order and concatenate to the expected text;
//! 3. the final `TurnResponse` equals — field for field — what the
//!    non-streaming `parse_response` produces for the equivalent complete
//!    body (the S1 contract-equivalence requirement);
//! 4. tool-call arguments split across >=3 delta fragments (and across TCP
//!    segment boundaries mid-JSON) reassemble and parse to identical JSON;
//! 5. a connection dropped mid-stream surfaces as a non-retryable
//!    `connection error: …` provider error in the retry.rs taxonomy —
//!    streaming is deliberately NOT auto-retried here (a retried stream
//!    would re-emit deltas); the policy is decided above this layer.

use modron_core::ModelProvider as _;
use modron_core::{Error, Message, StopReason, ToolSpec, TurnRequest, TurnResponse, Usage};
use modron_providers::retry::is_retryable;
use modron_providers::stream::StreamDelta;
use modron_providers::{AnthropicProvider, OpenAiCompatProvider};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::mpsc;

mod common;
use common::{spawn_sse_server, split_at_marker, sse_body, sse_response};

// ---------------------------------------------------------------------------
// Shared assertions and helpers
// ---------------------------------------------------------------------------

/// The S1 contract-equivalence assertion: the streaming `TurnResponse` must
/// equal the non-streaming parse of the equivalent complete body, field for
/// field (TurnResponse itself carries no PartialEq).
fn assert_same_turn(streaming: &TurnResponse, non_streaming: &TurnResponse) {
    assert_eq!(streaming.text, non_streaming.text, "text must match");
    assert_eq!(
        streaming.tool_calls.len(),
        non_streaming.tool_calls.len(),
        "tool call count must match"
    );
    for (streamed, parsed) in streaming.tool_calls.iter().zip(&non_streaming.tool_calls) {
        assert_eq!(streamed.id, parsed.id, "tool call id");
        assert_eq!(streamed.name, parsed.name, "tool call name");
        assert_eq!(streamed.arguments, parsed.arguments, "tool call arguments");
    }
    assert_eq!(streaming.usage, non_streaming.usage, "usage must match");
    assert_eq!(
        streaming.stop_reason, non_streaming.stop_reason,
        "stop reason must match"
    );
}

fn simple_request(model: &str) -> TurnRequest {
    TurnRequest {
        model: model.into(),
        system: "You are Modron.".into(),
        messages: vec![Message::user("list the rust files")],
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

fn drain(rx: &mut mpsc::UnboundedReceiver<StreamDelta>) -> Vec<StreamDelta> {
    let mut deltas = Vec::new();
    while let Ok(delta) = rx.try_recv() {
        deltas.push(delta);
    }
    deltas
}

// ---------------------------------------------------------------------------
// Anthropic fixtures
// ---------------------------------------------------------------------------

/// One text block + one tool_use whose arguments arrive as three
/// `input_json_delta` fragments. Equivalent non-streaming body: see
/// [`anthropic_equivalent_body`].
const ANTHROPIC_TOOL_STREAM: &[&str] = &[
    "event: message_start",
    r#"data: {"type":"message_start","message":{"id":"msg_01STREAM","type":"message","role":"assistant","model":"claude-sonnet-4-5","content":[],"stop_reason":null,"usage":{"input_tokens":2095,"output_tokens":1,"cache_read_input_tokens":1024,"cache_creation_input_tokens":512}}}"#,
    "",
    "event: content_block_start",
    r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
    "",
    "event: content_block_delta",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Let me check the repo."}}"#,
    "",
    "event: content_block_stop",
    r#"data: {"type":"content_block_stop","index":0}"#,
    "",
    "event: content_block_start",
    r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_01XYZ","name":"glob","input":{}}}"#,
    "",
    "event: content_block_delta",
    r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"pat"}}"#,
    "",
    "event: content_block_delta",
    r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"tern\": "}}"#,
    "",
    "event: content_block_delta",
    r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"*.rs\"}"}}"#,
    "",
    "event: content_block_stop",
    r#"data: {"type":"content_block_stop","index":1}"#,
    "",
    "event: message_delta",
    r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":121}}"#,
    "",
    "event: message_stop",
    r#"data: {"type":"message_stop"}"#,
];

/// A thinking block (reasoning deltas) followed by a text block.
const ANTHROPIC_THINKING_STREAM: &[&str] = &[
    "event: message_start",
    r#"data: {"type":"message_start","message":{"id":"msg_01THINK","type":"message","role":"assistant","model":"claude-sonnet-4-5","content":[],"stop_reason":null,"usage":{"input_tokens":300,"output_tokens":1}}}"#,
    "",
    "event: content_block_start",
    r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
    "",
    "event: content_block_delta",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Need to list files first"}}"#,
    "",
    "event: content_block_delta",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig=="}}"#,
    "",
    "event: content_block_stop",
    r#"data: {"type":"content_block_stop","index":0}"#,
    "",
    "event: content_block_start",
    r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
    "",
    "event: content_block_delta",
    r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Done."}}"#,
    "",
    "event: content_block_stop",
    r#"data: {"type":"content_block_stop","index":1}"#,
    "",
    "event: message_delta",
    r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":64}}"#,
    "",
    "event: message_stop",
    r#"data: {"type":"message_stop"}"#,
];

/// A stream cut off before `message_stop` — a connection drop mid-stream.
const ANTHROPIC_TRUNCATED_STREAM: &[&str] = &[
    "event: message_start",
    r#"data: {"type":"message_start","message":{"id":"msg_01DROP","type":"message","role":"assistant","model":"claude-sonnet-4-5","content":[],"stop_reason":null,"usage":{"input_tokens":10,"output_tokens":1}}}"#,
    "",
    "event: content_block_delta",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}"#,
];

/// A stream whose message_delta repeats the cumulative input and cache
/// counts (a shape newer API revisions may send). Usage must not be
/// double counted: input comes from message_start only (E3).
const ANTHROPIC_DELTA_USAGE_STREAM: &[&str] = &[
    "event: message_start",
    r#"data: {"type":"message_start","message":{"id":"msg_01DELTA","type":"message","role":"assistant","model":"claude-sonnet-4-5","content":[],"stop_reason":null,"usage":{"input_tokens":100,"output_tokens":1,"cache_read_input_tokens":40,"cache_creation_input_tokens":60}}}"#,
    "",
    "event: content_block_start",
    r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
    "",
    "event: content_block_delta",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}"#,
    "",
    "event: content_block_stop",
    r#"data: {"type":"content_block_stop","index":0}"#,
    "",
    "event: message_delta",
    r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"input_tokens":100,"output_tokens":7,"cache_read_input_tokens":40,"cache_creation_input_tokens":60}}"#,
    "",
    "event: message_stop",
    r#"data: {"type":"message_stop"}"#,
    "",
];

#[tokio::test]
async fn anthropic_streaming_usage_is_not_double_counted_from_message_delta() {
    let body = sse_body(ANTHROPIC_DELTA_USAGE_STREAM);
    let (addr, server) = spawn_sse_server(vec![(sse_response(&body), 0)]).await;
    let provider = AnthropicProvider::new("sk-test-key")
        .expect("provider")
        .with_base_url(format!("http://{addr}"));
    let (tx, _rx) = mpsc::unbounded_channel();
    let response = provider
        .turn_streaming(Arc::new(simple_request("claude-sonnet-4-5")), tx)
        .await
        .expect("streaming turn succeeds");
    server.await.expect("server task");

    assert_eq!(
        response.usage,
        Usage {
            // 100 uncached + 40 read + 60 write; the delta repeat is ignored.
            input_tokens: 200,
            output_tokens: 7,
            cached_input_tokens: 40,
        }
    );
}

fn anthropic_equivalent_body() -> Value {
    json!({
        "content": [
            {"type": "text", "text": "Let me check the repo."},
            {"type": "tool_use", "id": "toolu_01XYZ", "name": "glob", "input": {"pattern": "*.rs"}}
        ],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 2095, "output_tokens": 121, "cache_read_input_tokens": 1024, "cache_creation_input_tokens": 512}
    })
}

fn thinking_equivalent_body() -> Value {
    json!({
        "content": [
            // The non-streaming parser skips thinking blocks entirely; the
            // streaming path must too (reasoning goes to the delta channel).
            {"type": "thinking", "thinking": "Need to list files first", "signature": "sig=="},
            {"type": "text", "text": "Done."}
        ],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 300, "output_tokens": 64}
    })
}

#[tokio::test]
async fn anthropic_streaming_fixture_matches_non_streaming_contract() {
    let body = sse_body(ANTHROPIC_TOOL_STREAM);
    // Split mid-JSON: the TCP cut lands inside the second input_json_delta
    // fragment ("tern\": ").
    let segments = split_at_marker(&sse_response(&body), "tern\\");
    let (addr, server) = spawn_sse_server(segments).await;

    let provider = AnthropicProvider::new("sk-test-key")
        .expect("provider")
        .with_base_url(format!("http://{addr}"));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let response = provider
        .turn_streaming(Arc::new(simple_request("claude-sonnet-4-5")), tx)
        .await
        .expect("streaming turn succeeds");
    let recorded = server.await.expect("server task");

    // Wire shape of the REQUEST.
    assert_eq!(recorded.path, "/v1/messages");
    assert_eq!(recorded.x_api_key.as_deref(), Some("sk-test-key"));
    assert_eq!(recorded.anthropic_version.as_deref(), Some("2023-06-01"));
    assert_eq!(recorded.body.get("stream"), Some(&json!(true)));
    assert_eq!(recorded.body["tools"][0]["name"], "glob");

    // Deltas: one Text fragment, the COMPLETE tool call emitted
    // mid-stream (speculative-execution hook), then Done, in order.
    let deltas = drain(&mut rx);
    assert_eq!(
        deltas,
        vec![
            StreamDelta::Text("Let me check the repo.".into()),
            StreamDelta::ToolCallReady(modron_core::ToolCall {
                id: "toolu_01XYZ".into(),
                name: "glob".into(),
                arguments: json!({"pattern": "*.rs"}),
            }),
            StreamDelta::Done,
        ]
    );

    // Contract equivalence vs the non-streaming parse.
    let expected = modron_providers::anthropic::parse_response(anthropic_equivalent_body())
        .expect("non-streaming parse");
    assert_same_turn(&response, &expected);
    assert_eq!(response.tool_calls[0].arguments, json!({"pattern": "*.rs"}));
    assert_eq!(response.stop_reason, StopReason::ToolUse);
    assert_eq!(
        response.usage,
        Usage {
            // Anthropic input_tokens excludes cache; Usage.input includes
            // cache reads and writes (E3).
            input_tokens: 2095 + 1024 + 512,
            output_tokens: 121,
            cached_input_tokens: 1024
        }
    );
}

#[tokio::test]
async fn anthropic_streaming_thinking_surfaces_reasoning_only() {
    let body = sse_body(ANTHROPIC_THINKING_STREAM);
    let (addr, server) = spawn_sse_server(vec![(sse_response(&body), 0)]).await;
    let provider = AnthropicProvider::new("sk-test-key")
        .expect("provider")
        .with_base_url(format!("http://{addr}"));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let response = provider
        .turn_streaming(Arc::new(simple_request("claude-sonnet-4-5")), tx)
        .await
        .expect("streaming turn succeeds");
    let _ = server.await.expect("server task");

    assert_eq!(
        drain(&mut rx),
        vec![
            StreamDelta::Reasoning("Need to list files first".into()),
            StreamDelta::Text("Done.".into()),
            StreamDelta::Done,
        ]
    );
    let expected = modron_providers::anthropic::parse_response(thinking_equivalent_body())
        .expect("non-streaming parse");
    assert_same_turn(&response, &expected);
    assert_eq!(response.text, "Done.");
    assert!(response.tool_calls.is_empty());
}

#[tokio::test]
async fn anthropic_truncated_stream_is_connection_error_not_retryable() {
    // The server writes a partial stream and closes: no message_stop ever.
    let body = sse_body(ANTHROPIC_TRUNCATED_STREAM);
    let (addr, server) = spawn_sse_server(vec![(sse_response(&body), 0)]).await;
    let provider = AnthropicProvider::new("sk-test-key")
        .expect("provider")
        .with_base_url(format!("http://{addr}"));
    let (tx, _rx) = mpsc::unbounded_channel();
    let err = provider
        .turn_streaming(Arc::new(simple_request("claude-sonnet-4-5")), tx)
        .await
        .expect_err("truncated stream must fail, not return a partial turn");
    let _ = server.await.expect("server task");

    match &err {
        Error::Provider { provider, message } => {
            assert_eq!(provider, "anthropic");
            assert!(
                message.starts_with("connection error: "),
                "taxonomy prefix expected, got: {message}"
            );
        }
        other => panic!("expected Error::Provider, got: {other:?}"),
    }
    assert!(
        !is_retryable(&err),
        "streaming errors are not auto-retried here"
    );
}

// ---------------------------------------------------------------------------
// OpenAI-compatible fixtures
// ---------------------------------------------------------------------------

/// Content deltas + one tool_call whose `arguments` arrive in three
/// fragments + `finish_reason` + the usage-only chunk + `[DONE]`.
const OPENAI_TOOL_STREAM: &[&str] = &[
    r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1760000000,"model":"gpt-5-mini","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}"#,
    "",
    r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1760000000,"model":"gpt-5-mini","choices":[{"index":0,"delta":{"content":"Reading "},"finish_reason":null}]}"#,
    "",
    r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1760000000,"model":"gpt-5-mini","choices":[{"index":0,"delta":{"content":"src/lib.rs"},"finish_reason":null}]}"#,
    "",
    r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1760000000,"model":"gpt-5-mini","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_9","type":"function","function":{"name":"read","arguments":""}}]},"finish_reason":null}]}"#,
    "",
    r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1760000000,"model":"gpt-5-mini","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"pa"}}]},"finish_reason":null}]}"#,
    "",
    r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1760000000,"model":"gpt-5-mini","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\": "}}]},"finish_reason":null}]}"#,
    "",
    r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1760000000,"model":"gpt-5-mini","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"src/lib.rs\"}"}}]},"finish_reason":null}]}"#,
    "",
    r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1760000000,"model":"gpt-5-mini","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
    "",
    r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1760000000,"model":"gpt-5-mini","choices":[],"usage":{"prompt_tokens":1500,"completion_tokens":88,"prompt_tokens_details":{"cached_tokens":640}}}"#,
    "",
    "data: [DONE]",
];

/// The proxy-omits-usage shape: everything except the usage chunk.
const OPENAI_NO_USAGE_STREAM: &[&str] = &[
    r#"data: {"id":"chatcmpl-2","object":"chat.completion.chunk","created":1760000000,"model":"gpt-5-mini","choices":[{"index":0,"delta":{"content":"Hello there."},"finish_reason":null}]}"#,
    "",
    r#"data: {"id":"chatcmpl-2","object":"chat.completion.chunk","created":1760000000,"model":"gpt-5-mini","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
    "",
    "data: [DONE]",
];

/// A stream cut off before `[DONE]` — a connection drop mid-stream.
const OPENAI_TRUNCATED_STREAM: &[&str] = &[
    r#"data: {"id":"chatcmpl-3","object":"chat.completion.chunk","created":1760000000,"model":"gpt-5-mini","choices":[{"index":0,"delta":{"content":"partial"},"finish_reason":null}]}"#,
];

fn openai_equivalent_body() -> Value {
    json!({
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "Reading src/lib.rs",
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
    })
}

#[tokio::test]
async fn openai_streaming_fixture_matches_non_streaming_contract() {
    let body = sse_body(OPENAI_TOOL_STREAM);
    // TCP cut inside the second arguments fragment ("th\": ").
    let segments = split_at_marker(&sse_response(&body), "th\\");
    let (addr, server) = spawn_sse_server(segments).await;

    let provider = OpenAiCompatProvider::new("sk-test-key")
        .expect("provider")
        .with_base_url(format!("http://{addr}"));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let response = provider
        .turn_streaming(Arc::new(simple_request("gpt-5-mini")), tx)
        .await
        .expect("streaming turn succeeds");
    let recorded = server.await.expect("server task");

    // Wire shape of the REQUEST.
    assert_eq!(recorded.path, "/chat/completions");
    assert_eq!(
        recorded.authorization.as_deref(),
        Some("Bearer sk-test-key")
    );
    assert_eq!(recorded.body.get("stream"), Some(&json!(true)));
    assert_eq!(
        recorded.body.pointer("/stream_options/include_usage"),
        Some(&json!(true)),
        "usage must be requested; proxies that ignore it are handled below"
    );

    // Deltas: two content fragments then Done, in order.
    let deltas = drain(&mut rx);
    assert_eq!(
        deltas,
        vec![
            StreamDelta::Text("Reading ".into()),
            StreamDelta::Text("src/lib.rs".into()),
            StreamDelta::Done,
        ]
    );

    // Contract equivalence vs the non-streaming parse.
    let expected = modron_providers::openai_compat::parse_response(openai_equivalent_body())
        .expect("non-streaming parse");
    assert_same_turn(&response, &expected);
    assert_eq!(
        response.tool_calls[0].arguments,
        json!({"path": "src/lib.rs"})
    );
    assert_eq!(response.stop_reason, StopReason::ToolUse);
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: 1500,
            output_tokens: 88,
            cached_input_tokens: 640
        }
    );
}

#[tokio::test]
async fn openai_streaming_without_usage_chunk_reports_zeros() {
    let body = sse_body(OPENAI_NO_USAGE_STREAM);
    let (addr, server) = spawn_sse_server(vec![(sse_response(&body), 0)]).await;
    let provider = OpenAiCompatProvider::new("sk-test-key")
        .expect("provider")
        .with_base_url(format!("http://{addr}"));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let response = provider
        .turn_streaming(Arc::new(simple_request("gpt-5-mini")), tx)
        .await
        .expect("stream without usage still completes");
    let _ = server.await.expect("server task");

    assert_eq!(
        drain(&mut rx),
        vec![StreamDelta::Text("Hello there.".into()), StreamDelta::Done]
    );
    assert_eq!(response.text, "Hello there.");
    assert_eq!(response.stop_reason, StopReason::EndTurn);
    // Same honesty as the non-streaming path: zeros, never invented numbers.
    assert_eq!(response.usage, Usage::default());
    let expected = modron_providers::openai_compat::parse_response(json!({
        "choices": [{"message": {"role": "assistant", "content": "Hello there."}, "finish_reason": "stop"}]
    }))
    .expect("non-streaming parse");
    assert_same_turn(&response, &expected);
}

#[tokio::test]
async fn openai_truncated_stream_is_connection_error_not_retryable() {
    let body = sse_body(OPENAI_TRUNCATED_STREAM);
    let (addr, server) = spawn_sse_server(vec![(sse_response(&body), 0)]).await;
    let provider = OpenAiCompatProvider::new("sk-test-key")
        .expect("provider")
        .with_base_url(format!("http://{addr}"));
    let (tx, _rx) = mpsc::unbounded_channel();
    let err = provider
        .turn_streaming(Arc::new(simple_request("gpt-5-mini")), tx)
        .await
        .expect_err("truncated stream must fail, not return a partial turn");
    let _ = server.await.expect("server task");

    match &err {
        Error::Provider { provider, message } => {
            assert_eq!(provider, "openai-compat");
            assert!(
                message.starts_with("connection error: "),
                "taxonomy prefix expected, got: {message}"
            );
        }
        other => panic!("expected Error::Provider, got: {other:?}"),
    }
    assert!(
        !is_retryable(&err),
        "streaming errors are not auto-retried here"
    );
}
