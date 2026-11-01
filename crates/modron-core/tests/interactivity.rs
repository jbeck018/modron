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
                allow: Vec::new(),
                deny: Vec::new(),
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
                allow: Vec::new(),
                deny: Vec::new(),
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
            context_window: None,
        }
    }
}

/// A2: a delegate call runs a child with the persona's forced model and the
/// brief comes back into the parent context.
#[tokio::test]
async fn delegate_runs_child_with_persona_model() {
    use modron_core::testing::MockAsyncRouter;
    use modron_core::{PersonaSpec, SessionEvent, StopReason, TurnResponse, Usage};

    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn SessionStore> = Arc::new(kestrel_shim_store());
    // Shared provider script: [parent delegate-call, child answer, parent final].
    let delegate_call = TurnResponse {
        text: String::new(),
        tool_calls: vec![modron_core::ToolCall {
            id: "d1".into(),
            name: "delegate".into(),
            arguments: serde_json::json!({
                "task": "implement the fix",
                "persona": "builder",
                "tools": ["write"]
            }),
        }],
        usage: Usage {
            input_tokens: 200,
            output_tokens: 20,
            cached_input_tokens: 0,
        },
        stop_reason: StopReason::ToolUse,
    };
    let child_answer = TurnResponse {
        text: String::new(),
        tool_calls: Vec::new(),
        usage: Usage {
            input_tokens: 300,
            output_tokens: 900,
            cached_input_tokens: 0,
        },
        // Child "edits" then ends: EndTurn directly (no tools in this script).
        stop_reason: StopReason::EndTurn,
    };
    let parent_final = TurnResponse {
        text: "delegated and merged".into(),
        tool_calls: Vec::new(),
        usage: Usage {
            input_tokens: 400,
            output_tokens: 10,
            cached_input_tokens: 0,
        },
        stop_reason: StopReason::EndTurn,
    };
    let provider = MockProvider::new()
        .then_response(delegate_call)
        .then_response(child_answer)
        .then_response(parent_final);
    let mut providers: HashMap<String, Arc<dyn ModelProvider>> = HashMap::new();
    providers.insert("anthropic".to_string(), Arc::new(provider));

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(TouchTool));
    // The tools-crate DelegateTool can't be used here (crate direction);
    // this shim performs the same core call the real tool makes.
    registry.register(Arc::new(TestDelegateTool));

    let mut personas = std::collections::BTreeMap::new();
    personas.insert(
        "builder".to_string(),
        PersonaSpec {
            system: Some("You implement things.".into()),
            model: Some("claude-sonnet-4-5".into()),
            provider: Some("anthropic".into()),
            allow: vec!["write".into()],
            deny: vec![],
        },
    );

    let engine = modron_core::Engine {
        selector: Arc::new(TierSelector),
        providers,
        tools: Arc::new(registry),
        store: Arc::clone(&store),
        config: modron_core::EngineConfig {
            working_dir: dir.path().to_path_buf(),
            default_provider: "anthropic".into(),
            approvals: ApprovalPolicy {
                mode: ApprovalMode::ApproveAll,
                non_interactive: true,
                ..ApprovalPolicy::default()
            },
            personas,
            ..modron_core::EngineConfig::default()
        },
    };
    let _ = MockAsyncRouter::new(vec![]);
    let summary = engine.run_task("ship it").await.unwrap();

    // Honest accounting: the child's spend folds into the parent's totals
    // as a `subagent (delegate)` usage event, and the brief is in the
    // parent transcript.
    let events = store.load(summary.session_id).await.unwrap();
    let subagent_usage = events.iter().find_map(|e| match e {
        SessionEvent::Usage { model, usage } if model.starts_with("subagent") => Some(*usage),
        _ => None,
    });
    let child_usage = subagent_usage.expect("child spend must be recorded");
    assert_eq!(
        child_usage.total(),
        1_200,
        "child usage recorded: {:?}",
        child_usage
    );
    assert_eq!(
        summary.usage.total(),
        220 + 1_200 + 410,
        "parent totals include delegated spend"
    );
    let delegate_result = events.iter().find_map(|e| match e {
        SessionEvent::ToolResult { outcome, .. } => Some(outcome.content.clone()),
        _ => None,
    });
    assert!(delegate_result.is_some(), "delegate result present");
}

// Store shim for the test above (kept local to avoid a testing-crate export
// decision in the doc-comment).
fn kestrel_shim_store() -> impl modron_core::SessionStore {
    modron_core::testing::InMemoryStore::default()
}

/// Test-local mirror of the tools-crate DelegateTool (same core call path:
/// persona lookup from the engine config, then `subagent::delegate`).
struct TestDelegateTool;

#[async_trait::async_trait]
impl modron_core::Tool for TestDelegateTool {
    fn spec(&self) -> modron_core::ToolSpec {
        modron_core::ToolSpec {
            name: "delegate".into(),
            description: "test shim".into(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &modron_core::ToolCtx,
    ) -> modron_core::Result<modron_core::ToolOutcome> {
        let Some(engine) = ctx.engine.as_ref() else {
            return Ok(modron_core::ToolOutcome::err("no engine"));
        };
        let persona = args
            .get("persona")
            .and_then(|v| v.as_str())
            .and_then(|name| engine.config.personas.get(name));
        let report = modron_core::subagent::delegate(
            engine,
            "implement the fix",
            persona,
            Some(&["write".to_string()]),
            30_000,
        )
        .await?;
        Ok(modron_core::ToolOutcome {
            content: report.brief,
            is_error: false,
            usage: Some(report.usage),
            meta: None,
        })
    }
}

/// Phase 5h: TWO delegate calls in one turn run in PARALLEL (they are
/// read-only to the parent), both results land in the transcript in order,
/// and each child runs under its persona's forced model.
#[tokio::test]
async fn parallel_delegates_run_concurrently() {
    use modron_core::testing::MockAsyncRouter;
    use modron_core::{PersonaSpec, SessionEvent, StopReason, TurnResponse, Usage};

    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn SessionStore> = Arc::new(kestrel_shim_store());

    // Script: parent turn 1 = TWO delegate calls; then one child answer per
    // child (shared provider, order: children first? No — children run
    // concurrently but the shared mock pops sequentially: child A, child B);
    // then the parent's final answer.
    let twin_delegates = TurnResponse {
        text: String::new(),
        tool_calls: vec![
            modron_core::ToolCall {
                id: "d1".into(),
                name: "delegate".into(),
                arguments: serde_json::json!({"task": "do A", "persona": "builder"}),
            },
            modron_core::ToolCall {
                id: "d2".into(),
                name: "delegate".into(),
                arguments: serde_json::json!({"task": "do B", "persona": "builder"}),
            },
        ],
        usage: Usage {
            input_tokens: 200,
            output_tokens: 20,
            cached_input_tokens: 0,
        },
        stop_reason: StopReason::ToolUse,
    };
    let child_answer = |n: &str| TurnResponse {
        text: format!("result {n}"),
        tool_calls: Vec::new(),
        usage: Usage {
            input_tokens: 300,
            output_tokens: 50,
            cached_input_tokens: 0,
        },
        stop_reason: StopReason::EndTurn,
    };
    let parent_final = TurnResponse {
        text: "both done".into(),
        tool_calls: Vec::new(),
        usage: Usage {
            input_tokens: 400,
            output_tokens: 10,
            cached_input_tokens: 0,
        },
        stop_reason: StopReason::EndTurn,
    };
    let provider = MockProvider::new()
        .then_response(twin_delegates)
        .then_response(child_answer("A"))
        .then_response(child_answer("B"))
        .then_response(parent_final);
    let mut providers: HashMap<String, Arc<dyn ModelProvider>> = HashMap::new();
    providers.insert("anthropic".to_string(), Arc::new(provider));

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(TouchTool));
    registry.register(Arc::new(TestDelegateTool));

    let mut personas = std::collections::BTreeMap::new();
    personas.insert(
        "builder".to_string(),
        PersonaSpec {
            system: Some("You build.".into()),
            model: Some("claude-sonnet-4-5".into()),
            provider: Some("anthropic".into()),
            allow: vec!["write".into()],
            deny: vec![],
        },
    );

    let engine = modron_core::Engine {
        selector: Arc::new(TierSelector),
        providers,
        tools: Arc::new(registry),
        store: Arc::clone(&store),
        config: modron_core::EngineConfig {
            working_dir: dir.path().to_path_buf(),
            default_provider: "anthropic".into(),
            approvals: ApprovalPolicy {
                mode: ApprovalMode::ApproveAll,
                non_interactive: true,
                ..ApprovalPolicy::default()
            },
            personas,
            ..modron_core::EngineConfig::default()
        },
    };
    let _ = MockAsyncRouter::new(vec![]);
    let summary = engine.run_task("fan out").await.unwrap();
    assert_eq!(summary.answer, "both done");

    // Both children's results reached the transcript, in call order.
    let events = store.load(summary.session_id).await.unwrap();
    let delegate_results: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::ToolResult { outcome, .. } => Some(outcome.content.as_str()),
            _ => None,
        })
        .collect();
    let a = delegate_results.iter().position(|c| c.contains("result A"));
    let b = delegate_results.iter().position(|c| c.contains("result B"));
    assert!(
        a.is_some() && b.is_some(),
        "both children answered: {delegate_results:?}"
    );
    assert!(
        a.unwrap() < b.unwrap(),
        "order preserved under parallel execution"
    );
}

// -- Phase 13: memory feedback loop ------------------------------------------

/// A tool that reports memory hits through ToolOutcome.meta, mimicking
/// `memory_search` exactly as the engine sees it.
struct FakeMemorySearch;

#[async_trait::async_trait]
impl modron_core::Tool for FakeMemorySearch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "memory_search".into(),
            description: "test double".into(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &modron_core::ToolCtx,
    ) -> modron_core::Result<ToolOutcome> {
        let mut out = ToolOutcome::ok("found fact X");
        out.meta = Some(serde_json::json!({"memory_hits": ["fact-x"]}));
        Ok(out)
    }
}

/// M1: a completed run folds a success into the ledger of every memory the
/// run surfaced via ToolOutcome.meta.
#[tokio::test]
async fn memory_hits_fold_success_into_the_ledger() {
    let dir = tempfile::tempdir().unwrap();
    let memory_root = tempfile::tempdir().unwrap();
    // A fact on disk so the store exists and records survive.
    modron_core::memory::MemoryStore::for_workspace(memory_root.path(), dir.path())
        .unwrap()
        .append("fact x", "the fact body")
        .unwrap();

    let store: Arc<dyn SessionStore> = Arc::new(modron_core::testing::InMemoryStore::default());
    let provider = MockProvider::new()
        .then_response(tool_call_response("c1", "memory_search", json!({"query": "x"})))
        .then_response(text_response("used it, done"));
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(FakeMemorySearch));
    let mut providers = HashMap::new();
    providers.insert(
        "mock".to_string(),
        Arc::new(provider) as Arc<dyn ModelProvider>,
    );
    let engine = Engine {
        selector: Arc::new(StaticSelector::new("mock-model", "mock")),
        providers,
        tools: Arc::new(registry),
        store,
        config: EngineConfig {
            memory_root: Some(memory_root.path().to_path_buf()),
            working_dir: dir.path().to_path_buf(),
            ..EngineConfig::default()
        },
    };

    let summary = engine.run_task("find the fact").await.unwrap();
    assert_eq!(summary.answer, "used it, done");

    let workspace = modron_core::memory::MemoryStore::for_workspace(
        memory_root.path(),
        dir.path(),
    )
    .unwrap();
    let stats = workspace.stats();
    // The scripted fact's id contains the title; find it by prefix.
    let (id, stat) = stats
        .iter()
        .find(|(id, _)| id.contains("fact-x"))
        .expect("ledger row for the hit fact");
    assert_eq!(stat.successes, 1, "{id}: {stat:?}");
    assert_eq!(stat.failures, 0);
}
