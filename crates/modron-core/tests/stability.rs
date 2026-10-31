//! Byte-stability guarantee for the stable prompt prefix (PLAN.md §4.11,
//! LEDGER item K2). The engine must build `system` and `tools` once per run
//! and never mutate them across turns — that is what makes provider prompt
//! caches hit on every turn after the first.
//!
//! A multi-turn run (tool-call turn + final answer turn) is driven through
//! the real engine loop with a scripted [`MockProvider`]; the recorded
//! [`TurnRequest`]s are then compared byte-for-byte. No network, no real
//! filesystem (the working dir is an empty tempdir, so no memory files leak
//! into the prefix).

use async_trait::async_trait;
use modron_core::testing::{InMemoryStore, MockProvider, StaticSelector};
use modron_core::{
    Engine, EngineConfig, ModelProvider, Result, StopReason, Tool, ToolCall, ToolCtx, ToolOutcome,
    ToolRegistry, ToolSpec, TurnResponse, Usage,
};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "echo_tool".into(),
            description: "Echoes the working directory.".into(),
            parameters: json!({"type": "object"}),
        }
    }

    async fn execute(&self, _args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        Ok(ToolOutcome::ok(format!(
            "echo from {}",
            ctx.working_dir.display()
        )))
    }
}

fn engine(provider: Arc<dyn ModelProvider>, working_dir: PathBuf) -> Engine {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(EchoTool));
    let mut providers = HashMap::new();
    providers.insert("mock".to_string(), provider);
    Engine {
        selector: Arc::new(StaticSelector::new("mock-model", "mock")),
        providers,
        tools: Arc::new(registry),
        store: Arc::new(InMemoryStore::new()),
        config: EngineConfig {
            working_dir,
            ..EngineConfig::default()
        },
    }
}

fn tool_call_response(id: &str, name: &str, usage: Usage) -> TurnResponse {
    TurnResponse {
        text: String::new(),
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: json!({}),
        }],
        usage,
        stop_reason: StopReason::ToolUse,
    }
}

/// Serialize a request's `tools` the way the guarantee is defined: one
/// canonical JSON string per request.
fn tools_json(request: &modron_core::TurnRequest) -> String {
    serde_json::to_string(&request.tools).expect("ToolSpecs serialize")
}

#[tokio::test]
async fn system_and_tools_are_byte_identical_across_all_turns() {
    let provider = Arc::new(
        MockProvider::new()
            .then_response(tool_call_response(
                "call_1",
                "echo_tool",
                Usage {
                    input_tokens: 100,
                    output_tokens: 20,
                    cached_input_tokens: 0,
                },
            ))
            .then_response(TurnResponse {
                text: "All done.".into(),
                tool_calls: vec![],
                usage: Usage {
                    input_tokens: 150,
                    output_tokens: 30,
                    cached_input_tokens: 0,
                },
                stop_reason: StopReason::EndTurn,
            }),
    );
    let dir = tempfile::tempdir().expect("tempdir");
    let engine = engine(provider.clone(), dir.path().to_path_buf());

    let summary = engine.run_task("list the rust files").await.unwrap();
    assert_eq!(summary.answer, "All done.");

    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        2,
        "expected a tool-call turn plus a final turn"
    );
    for request in &requests {
        assert_eq!(
            request.model, "mock-model",
            "model pin must not vary per turn"
        );
    }

    // Non-vacuity: the compared prefix actually has content to be stable.
    let first = &requests[0];
    assert!(!first.system.is_empty(), "system prompt must be assembled");
    assert!(!first.tools.is_empty(), "tool specs must be present");

    // The guarantee: across ALL recorded requests, `system` is byte-identical
    // and `tools` (canonical JSON of the specs vec) is byte-identical.
    let system_bytes = first.system.as_bytes().to_vec();
    let tools_bytes = tools_json(first).into_bytes();
    for (turn, request) in requests.iter().enumerate().skip(1) {
        assert_eq!(
            request.system.as_bytes(),
            system_bytes.as_slice(),
            "system prompt changed at turn {turn}"
        );
        assert_eq!(
            tools_json(request).into_bytes(),
            tools_bytes.as_slice(),
            "tool specs changed at turn {turn}"
        );
    }
}

#[tokio::test]
async fn stability_holds_over_three_turns_with_repeated_tool_calls() {
    let provider = Arc::new(
        MockProvider::new()
            .then_response(tool_call_response("c1", "echo_tool", Usage::default()))
            .then_response(tool_call_response("c2", "echo_tool", Usage::default()))
            .then_response(TurnResponse {
                text: "Finished after two tool calls.".into(),
                tool_calls: vec![],
                usage: Usage::default(),
                stop_reason: StopReason::EndTurn,
            }),
    );
    let dir = tempfile::tempdir().expect("tempdir");
    let engine = engine(provider.clone(), dir.path().to_path_buf());
    engine.run_task("keep echoing").await.unwrap();

    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    let system_bytes = requests[0].system.as_bytes().to_vec();
    let tools_bytes = tools_json(&requests[0]).into_bytes();
    for (turn, request) in requests.iter().enumerate() {
        assert_eq!(
            request.system.as_bytes(),
            system_bytes.as_slice(),
            "turn {turn}"
        );
        assert_eq!(
            tools_json(request).into_bytes(),
            tools_bytes.as_slice(),
            "turn {turn}"
        );
        // Messages grow turn over turn; the prefix must not.
        assert!(
            request.messages.len() > turn,
            "transcript must grow at turn {turn}"
        );
    }
}
