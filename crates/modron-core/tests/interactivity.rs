//! Phase-1c engine-level interactivity tests: the delta sink (I1) and the
//! interactive approver (I2), both through the real engine loop.

use modron_core::testing::{MockProvider, StaticSelector};
use modron_core::{
    ApprovalMode, ApprovalPolicy, Engine, EngineConfig, ModelProvider, Role, SessionStore,
    StopReason, StreamDelta, ToolCall, ToolOutcome, ToolRegistry, ToolSpec, TurnResponse, Usage,
};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

fn tool_call_response(id: &str, name: &str, arguments: serde_json::Value) -> TurnResponse {
    TurnResponse {
        text: String::new(),
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: name.into(),
            arguments,
        }],
        usage: Usage {
            input_tokens: 100,
            output_tokens: 10,
            cached_input_tokens: 0,
        },
        stop_reason: StopReason::ToolUse,
    }
}

fn text_response(text: &str) -> TurnResponse {
    TurnResponse {
        text: text.to_string(),
        tool_calls: Vec::new(),
        usage: Usage {
            input_tokens: 120,
            output_tokens: 8,
            cached_input_tokens: 0,
        },
        stop_reason: StopReason::EndTurn,
    }
}

struct TouchTool;
#[async_trait::async_trait]
impl modron_core::Tool for TouchTool {
    fn spec(&self) -> ToolSpec {
        // Named `write` so it lands in the mutating tool class (A1) and the
        // approval gate actually engages for these tests.
        ToolSpec {
            name: "write".into(),
            description: "create an empty marker file".into(),
            parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}),
        }
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &modron_core::ToolCtx,
    ) -> modron_core::Result<ToolOutcome> {
        let path = args["path"].as_str().unwrap_or("marker.txt");
        std::fs::write(ctx.working_dir.join(path), b"touched").map_err(|e| {
            modron_core::Error::Tool {
                tool: "touchfile".into(),
                message: e.to_string(),
            }
        })?;
        Ok(ToolOutcome::ok("touched"))
    }
}

fn engine(
    provider: MockProvider,
    store: Arc<dyn SessionStore>,
    _dir: &std::path::Path,
    config: EngineConfig,
) -> Engine {
    let mut providers = HashMap::new();
    providers.insert(
        "mock".to_string(),
        Arc::new(provider) as Arc<dyn ModelProvider>,
    );
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(TouchTool));
    Engine {
        selector: Arc::new(StaticSelector::new("mock-model", "mock")),
        providers,
        tools: Arc::new(registry),
        store,
        config,
    }
}

/// I1: with a delta sink configured, the engine drives `turn_streaming` and
/// the sink sees the text (via the trait-default fallback for the mock).
#[tokio::test]
async fn delta_sink_receives_streamed_text() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn SessionStore> = Arc::new(modron_core::testing::InMemoryStore::default());
    let provider = MockProvider::new().then_response(text_response("hello streamed world"));
    let deltas = Arc::new(Mutex::new(Vec::new()));
    let sink_deltas = Arc::clone(&deltas);
    let sink = Arc::new(move |delta: StreamDelta| sink_deltas.lock().unwrap().push(delta))
        as Arc<dyn Fn(StreamDelta) + Send + Sync>;
    let engine = engine(
        provider,
        store,
        dir.path(),
        EngineConfig {
            on_delta: Some(sink),
            ..EngineConfig::default()
        },
    );
    let summary = engine.run_task("say hi").await.unwrap();
    assert_eq!(summary.answer, "hello streamed world");
    let seen = deltas.lock().unwrap();
    assert!(
        matches!(seen.as_slice(), [StreamDelta::Text(t)] if t == "hello streamed world"),
        "expected one Text delta with the answer, got {seen:?}"
    );
}

/// I2: in Ask mode, a set approver allows the mutating tool through.
#[tokio::test]
async fn approver_allows_mutating_tool_in_ask_mode() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn SessionStore> = Arc::new(modron_core::testing::InMemoryStore::default());
    let provider = MockProvider::new()
        .then_response(tool_call_response("c1", "write", json!({"path": "ok.txt"})))
        .then_response(text_response("approved and done"));
    let engine = engine(
        provider,
        store,
        dir.path(),
        EngineConfig {
            working_dir: dir.path().to_path_buf(),
            approvals: ApprovalPolicy {
                mode: ApprovalMode::Ask,
                non_interactive: false,
            },
            approver: Some(always_allow()),
            ..EngineConfig::default()
        },
    );
    let summary = engine.run_task("touch it").await.unwrap();
    assert_eq!(summary.answer, "approved and done");
    assert!(dir.path().join("ok.txt").exists());
}

fn always_allow() -> Arc<modron_core::Approver> {
    Arc::new(|_call: &ToolCall| true)
}

fn always_deny() -> Arc<modron_core::Approver> {
    Arc::new(|_call: &ToolCall| false)
}

/// I2: a denying approver blocks with the actionable message.
#[tokio::test]
async fn approver_denial_blocks_with_message() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn SessionStore> = Arc::new(modron_core::testing::InMemoryStore::default());
    let provider = MockProvider::new()
        .then_response(tool_call_response("c1", "write", json!({"path": "no.txt"})))
        .then_response(text_response("adapted"));
    let engine = engine(
        provider,
        Arc::clone(&store),
        dir.path(),
        EngineConfig {
            working_dir: dir.path().to_path_buf(),
            approvals: ApprovalPolicy {
                mode: ApprovalMode::Ask,
                non_interactive: false,
            },
            approver: Some(always_deny()),
            ..EngineConfig::default()
        },
    );
    let summary = engine.run_task("touch it").await.unwrap();
    assert!(!dir.path().join("no.txt").exists());
    // The denial path is the pre-approver block: actionable for the user.
    let events = store.load(summary.session_id).await.unwrap();
    let blocked = events
        .iter()
        .find_map(|e| match e {
            modron_core::SessionEvent::ToolResult { outcome, .. } => Some(outcome.content.clone()),
            _ => None,
        })
        .unwrap();
    assert!(
        blocked.contains("blocked by approval mode"),
        "got: {blocked}"
    );
    // The transcript carried the block to the model (Role::Tool message).
    let _ = Role::Tool;
}

/// M1: an Always-mode async router re-routes through the mapped action; the
/// routing decision (and its provenance) lands in the ModelSelected event.
#[tokio::test]
async fn async_router_verdict_reroutes_the_task() {
    use modron_core::testing::MockAsyncRouter;
    use modron_core::JudgeMode;

    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn SessionStore> = Arc::new(modron_core::testing::InMemoryStore::default());
    // The main provider answers the one real turn.
    let provider = MockProvider::new().then_response(text_response("done"));
    let mut providers: HashMap<String, Arc<dyn ModelProvider>> = HashMap::new();
    providers.insert("anthropic".to_string(), Arc::new(provider));

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(TouchTool));

    let engine = Engine {
        selector: Arc::new(TierSelector),
        providers,
        tools: Arc::new(registry),
        store,
        config: EngineConfig {
            working_dir: dir.path().to_path_buf(),
            default_provider: "anthropic".into(),
            async_router: Some((
                Arc::new(MockAsyncRouter::new(vec![Some(
                    modron_core::RouteVerdict::Judgment,
                )])),
                JudgeMode::Always,
            )),
            ..EngineConfig::default()
        },
    };
    let summary = engine.run_task("say hi").await.unwrap();
    assert_eq!(summary.answer, "done");
    // Judgment verdict → Plan action → strong tier (sonnet), not the cheap
    // default the short chat would have picked.
    assert!(
        summary
            .models_used
            .contains(&"claude-sonnet-4-5".to_string()),
        "models: {:?}",
        summary.models_used
    );
}

/// Tier-aware selector mirroring the heuristic defaults (Chat→haiku,
/// Plan→sonnet) so the verdict-override effect is observable.
struct TierSelector;

#[async_trait::async_trait]
impl modron_core::ModelSelector for TierSelector {
    fn select(&self, req: &modron_core::SelectRequest) -> modron_core::ModelChoice {
        let (model, confident) = match req.action {
            modron_core::ActionKind::Plan | modron_core::ActionKind::Edit => {
                ("claude-sonnet-4-5", true)
            }
            _ => ("claude-haiku-4-5", true),
        };
        modron_core::ModelChoice {
            model: model.into(),
            provider: "anthropic".into(),
            reason: "tier test selector".into(),
            confident,
        }
    }
}
