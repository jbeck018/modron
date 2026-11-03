//! Phase 2a acceptance tests: the usage dashboard (D1) and the verify tool
//! through the real engine (D3). Config-section parsing is covered in
//! config.rs unit tests; the prompt-budget gate lives in budget.rs.

use modron_core::testing::{MockProvider, StaticSelector};
use modron_core::{
    ApprovalMode, ApprovalPolicy, Engine, EngineConfig, ModelProvider, SessionStore, StopReason,
    ToolCall, ToolRegistry, TurnResponse, Usage,
};
use modron_store::JsonlSessionStore;
use modron_tools::{default_registry, VerifyTool};
use serde_json::json;
use std::collections::HashMap;
use std::process::Command;
use std::sync::Arc;

fn modron_bin() -> &'static str {
    env!("CARGO_BIN_EXE_modron")
}

/// D1: after a real demo run, `modron usage` aggregates the persisted
/// usage events into per-model rows with totals and a cost figure.
#[test]
fn usage_dashboard_aggregates_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    // Isolate the config: the developer's real config.toml may carry a
    // saved default model (the /model menu's save-as-default), which
    // would change the demo's model row.
    let config_dir = dir.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    let run = Command::new(modron_bin())
        .env("MODRON_CONFIG", config_dir.to_str().unwrap())
        .args([
            "demo",
            "--provider",
            "mock",
            "--approve-all",
            "--sessions-dir",
            sessions.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(run.status.success());

    let out = Command::new(modron_bin())
        .env("MODRON_CONFIG", config_dir.to_str().unwrap())
        .args(["usage", "--sessions-dir", sessions.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("usage across 1 sessions (1 with token data)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("claude-sonnet-5-5"),
        "per-model row: {stdout}"
    );
    assert!(stdout.contains("input"), "table present: {stdout}");
    assert!(stdout.contains("output"), "output col: {stdout}");
    assert!(stdout.contains('$'), "estimated cost: {stdout}");
    assert!(stdout.contains("%"), "cache-hit present: {stdout}");
}

/// D1: empty store → honest message, exit 0.
#[test]
fn usage_on_empty_store_is_honest() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(modron_bin())
        .args([
            "usage",
            "--sessions-dir",
            dir.path().join("s").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("no sessions"), "{stdout}");
}

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

/// D3: the verify tool runs the configured command through the real engine
/// and the model sees the verdict.
#[tokio::test]
async fn verify_tool_runs_through_the_engine() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("marker.txt"), "payload").unwrap();
    let store: Arc<dyn SessionStore> = Arc::new(JsonlSessionStore::new(dir.path().join("s")));
    let provider = MockProvider::new()
        .then_response(tool_call_response("c1", "verify", json!({})))
        .then_response(text_response("verified"));
    let mut providers: HashMap<String, Arc<dyn ModelProvider>> = HashMap::new();
    providers.insert("mock".to_string(), Arc::new(provider));

    let mut registry: ToolRegistry = default_registry();
    registry.register(Arc::new(VerifyTool::new("cat marker.txt")));

    let engine = Engine {
        selector: Arc::new(StaticSelector::new("mock-model", "mock")),
        providers,
        tools: Arc::new(registry),
        store,
        config: EngineConfig {
            working_dir: dir.path().to_path_buf(),
            approvals: ApprovalPolicy {
                mode: ApprovalMode::ApproveAll,
                non_interactive: true,
                allow: Vec::new(),
                deny: Vec::new(),
            },
            ..EngineConfig::default()
        },
    };
    let summary = engine.run_task("run the checks").await.unwrap();
    assert_eq!(summary.answer, "verified");
}

/// D3: a failing verify command reaches the model as an error outcome.
#[tokio::test]
async fn verify_failure_is_visible_to_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn SessionStore> = Arc::new(JsonlSessionStore::new(dir.path().join("s")));
    // The finish gate now also fires at end-of-run (verify is a mutating
    // tool and its first run failed), forcing one more scripted turn.
    let provider = MockProvider::new()
        .then_response(tool_call_response("c1", "verify", json!({})))
        .then_response(text_response("will fix"))
        .then_response(text_response("fixed"))
        .then_repeat_last();
    let mut providers: HashMap<String, Arc<dyn ModelProvider>> = HashMap::new();
    providers.insert("mock".to_string(), Arc::new(provider));

    let mut registry: ToolRegistry = default_registry();
    registry.register(Arc::new(VerifyTool::new(
        "echo '2 tests failed' >&2; exit 1",
    )));

    let engine = Engine {
        selector: Arc::new(StaticSelector::new("mock-model", "mock")),
        providers,
        tools: Arc::new(registry),
        store: Arc::clone(&store),
        config: EngineConfig {
            working_dir: dir.path().to_path_buf(),
            approvals: ApprovalPolicy {
                mode: ApprovalMode::ApproveAll,
                non_interactive: true,
                allow: Vec::new(),
                deny: Vec::new(),
            },
            ..EngineConfig::default()
        },
    };
    let summary = engine.run_task("run the checks").await.unwrap();
    let events = store.load(summary.session_id).await.unwrap();
    let verdict = events
        .iter()
        .find_map(|e| match e {
            modron_core::SessionEvent::ToolResult { outcome, .. } => Some(outcome.clone()),
            _ => None,
        })
        .unwrap();
    assert!(verdict.is_error);
    assert!(verdict.content.contains("2 tests failed"));
    assert!(verdict.content.contains("exit code: 1"));
    // The gate fed the failure back; with a permanently failing command
    // it exhausts its rounds and ends with the visible marker.
    assert!(summary.answer.starts_with("fixed"), "{:?}", summary.answer);
    assert!(
        summary.answer.contains("verification still failing"),
        "{:?}",
        summary.answer
    );
    assert!(summary.turns >= 4);
}
