//! Offline end-to-end engine tests (LEDGER C1). A mock provider scripts a
//! tool-calling run through the real engine loop with an in-memory store and
//! a dummy tool registry. No network, no real filesystem.

use async_trait::async_trait;
use modron_core::testing::{InMemoryStore, MockProvider, StaticSelector};
use modron_core::{
    Engine, EngineConfig, Error, ModelProvider, Result, Role, SessionEvent, StopReason, Tool,
    ToolCall, ToolCtx, ToolOutcome, ToolRegistry, ToolSpec, TurnResponse, Usage,
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

struct BoomTool;

#[async_trait]
impl Tool for BoomTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "boom_tool".into(),
            description: "Always fails to run.".into(),
            parameters: json!({"type": "object"}),
        }
    }

    async fn execute(&self, _args: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutcome> {
        Err(Error::Other(anyhow::anyhow!("kaboom")))
    }
}

fn registry(with_boom: bool) -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(EchoTool));
    if with_boom {
        registry.register(Arc::new(BoomTool));
    }
    Arc::new(registry)
}

fn engine(
    provider: Arc<dyn ModelProvider>,
    config: EngineConfig,
    tools: Arc<ToolRegistry>,
) -> Engine {
    let mut providers = HashMap::new();
    providers.insert("mock".to_string(), provider);
    Engine {
        selector: Arc::new(StaticSelector::new("mock-model", "mock")),
        providers,
        tools,
        store: Arc::new(InMemoryStore::new()),
        config,
    }
}

fn base_config() -> EngineConfig {
    EngineConfig {
        working_dir: PathBuf::from("/tmp/modron-engine-test"),
        ..EngineConfig::default()
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

#[tokio::test]
async fn full_tool_loop_end_to_end() {
    let provider = Arc::new(
        MockProvider::new()
            .then_response(tool_call_response(
                "call_1",
                "echo_tool",
                Usage {
                    input_tokens: 100,
                    output_tokens: 20,
                    cached_input_tokens: 10,
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
    let engine = engine(provider.clone(), base_config(), registry(false));
    let summary = engine.run_task("list the rust files").await.unwrap();

    // Summary accounting.
    assert_eq!(summary.answer, "All done.");
    assert_eq!(summary.turns, 2);
    assert_eq!(summary.tool_calls, 1);
    assert_eq!(summary.usage.input_tokens, 250);
    assert_eq!(summary.usage.output_tokens, 50);
    assert_eq!(summary.usage.cached_input_tokens, 10);
    assert_eq!(summary.models_used, vec!["mock-model".to_string()]);

    // Transcript as seen by the provider: task, assistant+tool_calls, tool result.
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].messages.len(), 1);
    assert_eq!(requests[0].messages[0].role, Role::User);
    assert!(
        !requests[0].system.is_empty(),
        "system prompt must be assembled"
    );
    assert_eq!(requests[0].tools.len(), 1);
    assert_eq!(requests[1].messages.len(), 3);
    assert_eq!(requests[1].messages[1].role, Role::Assistant);
    assert_eq!(requests[1].messages[1].tool_calls.len(), 1);
    assert_eq!(requests[1].messages[2].role, Role::Tool);
    assert_eq!(
        requests[1].messages[2].tool_call_id.as_deref(),
        Some("call_1")
    );
    assert!(requests[1].messages[2]
        .content
        .contains("echo from /tmp/modron-engine-test"));

    // Session events appended as they happened.
    // Session events appended as they happened. TurnTrace observability
    // records ride along between the structural events — assert on the
    // structural subsequence so the ordering intent stays readable.
    let events = engine.store.load(summary.session_id).await.unwrap();
    let structural: Vec<&SessionEvent> = events
        .iter()
        .filter(|e| !matches!(e, SessionEvent::TurnTrace { .. }))
        .collect();
    assert_eq!(structural.len(), 8, "Task + 2×(MS,U,M) + TR");
    assert!(
        matches!(structural[0], SessionEvent::Task { ref text, .. } if text == "list the rust files")
    );
    assert!(
        matches!(structural[1], SessionEvent::ModelSelected { ref model, .. } if model == "mock-model")
    );
    assert!(matches!(structural[2], SessionEvent::Usage { .. }));
    assert!(
        matches!(structural[3], SessionEvent::Message { ref message, ..
        }
        if message.role == Role::Assistant && message.tool_calls.len() == 1)
    );
    assert!(
        matches!(structural[4], SessionEvent::ToolResult { ref call_id, ref outcome }
        if call_id == "call_1" && !outcome.is_error)
    );
    assert!(matches!(structural[5], SessionEvent::ModelSelected { .. }));
    assert!(matches!(structural[6], SessionEvent::Usage { .. }));
    assert!(
        matches!(structural[7], SessionEvent::Message { ref message, ..
        }
        if message.role == Role::Assistant && message.content == "All done.")
    );

    // Session title = first line of the task.
    let listed = engine.store.list().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].title, "list the rust files");
}

#[tokio::test]
async fn stops_at_max_turns() {
    let provider = Arc::new(
        MockProvider::new()
            .then_response(tool_call_response("c1", "echo_tool", Usage::default()))
            .then_response(tool_call_response("c2", "echo_tool", Usage::default()))
            .then_response(tool_call_response("c3", "echo_tool", Usage::default())),
    );
    let mut config = base_config();
    config.max_turns = 2;
    let engine = engine(provider.clone(), config, registry(false));

    let summary = engine.run_task("loop forever").await.unwrap();
    assert_eq!(summary.turns, 2);
    assert_eq!(summary.tool_calls, 2);
    assert_eq!(summary.answer, "");
    assert_eq!(
        provider.requests().len(),
        2,
        "no third turn after max_turns"
    );
}

#[tokio::test]
async fn preflight_budget_error_is_budget() {
    let provider = Arc::new(MockProvider::new().then_text("never reached"));
    let mut config = base_config();
    config.max_input_tokens = 5; // the assembled system prompt alone is larger
    let engine = engine(provider, config, registry(false));

    let err = engine.run_task("hello").await.unwrap_err();
    assert!(matches!(err, Error::Budget(_)), "got: {err}");
}

#[tokio::test]
async fn cumulative_budget_stops_the_loop_gracefully() {
    let provider = Arc::new(
        MockProvider::new()
            .then_response(tool_call_response(
                "c1",
                "echo_tool",
                Usage {
                    input_tokens: 90,
                    output_tokens: 20,
                    cached_input_tokens: 0,
                },
            ))
            .then_response(tool_call_response("c2", "echo_tool", Usage::default()))
            .then_text("never reached"),
    );
    let mut config = base_config();
    config.max_total_tokens = 100;
    let engine = engine(provider.clone(), config, registry(false));

    let summary = engine.run_task("spendy task").await.unwrap();
    assert_eq!(
        summary.turns, 1,
        "cumulative cap stops after the over-budget turn"
    );
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(summary.usage.total(), 110);
}

#[tokio::test]
async fn unknown_tool_and_tool_crash_become_error_outcomes() {
    let provider = Arc::new(
        MockProvider::new()
            .then_response(tool_call_response("c1", "missing_tool", Usage::default()))
            .then_response(tool_call_response("c2", "boom_tool", Usage::default()))
            .then_text("recovered"),
    );
    let engine = engine(provider.clone(), base_config(), registry(true));

    let summary = engine.run_task("exercise failures").await.unwrap();
    assert_eq!(summary.tool_calls, 2);
    assert_eq!(summary.answer, "recovered");

    let requests = provider.requests();
    assert!(
        requests[1].messages[2].content.contains("unknown tool"),
        "got: {}",
        requests[1].messages[2].content
    );
    assert!(
        requests[2].messages[4].content.contains("failed to run"),
        "got: {}",
        requests[2].messages[4].content
    );
}

#[tokio::test]
async fn unregistered_provider_falls_back_to_default() {
    let provider = Arc::new(MockProvider::new().then_text("fallback ok"));
    let mut providers: HashMap<String, Arc<dyn ModelProvider>> = HashMap::new();
    providers.insert("mock".to_string(), provider);
    let mut config = base_config();
    config.default_provider = "mock".into();
    config.default_model = "mock-model".into();
    let engine = Engine {
        selector: Arc::new(StaticSelector::new("m", "nonexistent")),
        providers,
        tools: registry(false),
        store: Arc::new(InMemoryStore::new()),
        config,
    };

    let summary = engine.run_task("fallback").await.unwrap();
    assert_eq!(summary.answer, "fallback ok");
    // Fell back to the configured default model.
    assert_eq!(summary.models_used, vec!["mock-model".to_string()]);
}

#[tokio::test]
async fn empty_task_is_config_error() {
    let engine = engine(
        Arc::new(MockProvider::new()),
        base_config(),
        registry(false),
    );
    let err = engine.run_task("   ").await.unwrap_err();
    assert!(matches!(err, Error::Config(_, _)), "got: {err}");
}

#[tokio::test]
async fn resume_reconstructs_transcript_and_continues_same_session() {
    // Fresh run: one tool call, then a final answer.
    let provider = Arc::new(
        MockProvider::new()
            .then_response(tool_call_response(
                "r1",
                "echo_tool",
                Usage {
                    input_tokens: 50,
                    output_tokens: 10,
                    cached_input_tokens: 0,
                },
            ))
            .then_text("first answer"),
    );
    let engine = engine(provider.clone(), base_config(), registry(false));
    let summary = engine.run_task("start work").await.unwrap();
    assert_eq!(summary.answer, "first answer");
    let events_before = engine.store.load(summary.session_id).await.unwrap().len();

    // Resume with a NEW provider in the SAME session id.
    let provider2 = Arc::new(MockProvider::new().then_text("second answer"));
    let mut providers: HashMap<String, Arc<dyn ModelProvider>> = HashMap::new();
    providers.insert(
        "mock".to_string(),
        provider2.clone() as Arc<dyn ModelProvider>,
    );
    let engine2 = Engine {
        selector: Arc::new(StaticSelector::new("mock-model", "mock")),
        providers,
        tools: registry(false),
        store: Arc::clone(&engine.store),
        config: base_config(),
    };
    let summary2 = engine2
        .run_task_resuming(summary.session_id, "keep going")
        .await
        .unwrap();

    assert_eq!(
        summary2.session_id, summary.session_id,
        "resume continues the SAME session"
    );
    assert_eq!(summary2.answer, "second answer");

    // The resumed turn saw the full reconstructed transcript: original task,
    // assistant tool call, tool result, first final message, new task.
    let requests = provider2.requests();
    assert_eq!(requests.len(), 1);
    let messages = &requests[0].messages;
    assert_eq!(
        messages.len(),
        5,
        "task + assistant(call) + tool + assistant + new task"
    );
    assert_eq!(messages[0].content, "start work");
    assert_eq!(messages[1].tool_calls.len(), 1);
    assert_eq!(messages[2].role, Role::Tool);
    assert_eq!(messages[2].tool_call_id.as_deref(), Some("r1"));
    assert_eq!(messages[3].content, "first answer");
    assert_eq!(messages[4].role, Role::User);
    assert_eq!(messages[4].content, "keep going");

    // Events keep accumulating in the same session log (+ TurnTrace
    // records for the resumed turn and its terminal stop label).
    let events_after = engine2.store.load(summary.session_id).await.unwrap();
    assert_eq!(
        events_after.len(),
        events_before + 6,
        "Task + ModelSelected + Usage + TurnTrace + Message + terminal TurnTrace"
    );
    assert!(
        matches!(events_after.first(), Some(SessionEvent::Task { ref text }) if text == "start work")
    );
    assert!(events_after
        .iter()
        .any(|e| matches!(e, SessionEvent::Message { ref message, .. }
                if message.content == "second answer")));
}

#[tokio::test]
async fn resume_of_unknown_session_is_session_error() {
    let engine = engine(
        Arc::new(MockProvider::new()),
        base_config(),
        registry(false),
    );
    let ghost = uuid::Uuid::new_v4();
    let err = engine.run_task_resuming(ghost, "hi").await.unwrap_err();
    assert!(matches!(err, Error::Session(_)), "got: {err}");
}

#[tokio::test]
async fn resume_with_empty_task_is_config_error() {
    let engine = engine(
        Arc::new(MockProvider::new().then_text("seeded")),
        base_config(),
        registry(false),
    );
    let summary = engine.run_task("seed").await.unwrap();
    let err = engine
        .run_task_resuming(summary.session_id, "  ")
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Config(_, _)), "got: {err}");
}
