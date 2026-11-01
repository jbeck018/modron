//! V9 — Phase 1b end-to-end acceptance tests (approval gate, sandbox,
//! hooks, rewind, Z.ai factory). All offline; the live Seatbelt behavior
//! additionally has a real-kernel test in `modron-core/src/sandbox.rs`.

use modron_core::testing::{MockProvider, StaticSelector};
use modron_core::{
    hooks::HookConfig, ApprovalMode, ApprovalPolicy, Engine, EngineConfig, ModelProvider,
    SandboxPolicy, SessionEvent, SessionStore, StopReason, ToolCall, TurnResponse, Usage,
};
use modron_providers::provider_from_env;
use modron_store::JsonlSessionStore;
use modron_tools::default_registry;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

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

fn engine_with(
    provider: MockProvider,
    store: Arc<dyn SessionStore>,
    dir: &std::path::Path,
    customize: impl FnOnce(&mut EngineConfig),
) -> Engine {
    let mut providers = HashMap::new();
    providers.insert(
        "mock".to_string(),
        Arc::new(provider) as Arc<dyn ModelProvider>,
    );
    let mut config = EngineConfig {
        working_dir: dir.to_path_buf(),
        ..EngineConfig::default()
    };
    customize(&mut config);
    Engine {
        selector: Arc::new(StaticSelector::new("mock-model", "mock")),
        providers,
        tools: Arc::new(default_registry()),
        store,
        config,
    }
}

/// A1: in Ask mode (the default), a `write` call is blocked with an
/// actionable message and the run still completes.
#[tokio::test]
async fn approval_gate_blocks_write_and_the_model_sees_why() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn SessionStore> =
        Arc::new(JsonlSessionStore::new(dir.path().join("sessions")));
    let provider = MockProvider::new()
        .then_response(tool_call_response(
            "c1",
            "write",
            json!({"path": "x.txt", "content": "hi"}),
        ))
        .then_response(text_response("acknowledged the block"));
    let engine = engine_with(provider, Arc::clone(&store), dir.path(), |_| {});

    let summary = engine.run_task("write a file").await.unwrap();
    assert_eq!(summary.answer, "acknowledged the block");
    assert_eq!(
        summary.tool_calls, 1,
        "the call was dispatched (as a block), not skipped"
    );
    assert!(
        !dir.path().join("x.txt").exists(),
        "the write must NOT have happened"
    );

    // The model saw the block and can adapt.
    let events = store.load(summary.session_id).await.unwrap();
    let blocked = events.iter().find_map(|e| match e {
        SessionEvent::ToolResult { outcome, .. } => Some(outcome.content.clone()),
        _ => None,
    });
    let blocked = blocked.expect("a tool result exists");
    assert!(
        blocked.contains("blocked by approval mode"),
        "got: {blocked}"
    );
    assert!(
        blocked.contains("--approve-all"),
        "message is actionable: {blocked}"
    );
}

/// A1: ApproveAll executes the same call.
#[tokio::test]
async fn approve_all_executes_mutating_tools() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn SessionStore> =
        Arc::new(JsonlSessionStore::new(dir.path().join("sessions")));
    let provider = MockProvider::new()
        .then_response(tool_call_response(
            "c1",
            "write",
            json!({"path": "x.txt", "content": "hi"}),
        ))
        .then_response(text_response("done"));
    let engine = engine_with(provider, Arc::clone(&store), dir.path(), |cfg| {
        cfg.approvals = ApprovalPolicy {
            mode: ApprovalMode::ApproveAll,
            non_interactive: true,
            allow: Vec::new(),
            deny: Vec::new(),
        };
    });

    let summary = engine.run_task("write a file").await.unwrap();
    assert_eq!(summary.answer, "done");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("x.txt")).unwrap(),
        "hi"
    );
}

/// A2 (macOS): under Seatbelt a network touch fails while a workspace write
/// succeeds, through the REAL bash tool via the engine.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn seatbelt_sandbox_blocks_network_allows_workspace_writes() {
    if std::process::Command::new("which")
        .arg("sandbox-exec")
        .output()
        .map(|o| o.stdout.is_empty())
        .unwrap_or(true)
    {
        eprintln!("skipping: sandbox-exec not available");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn SessionStore> =
        Arc::new(JsonlSessionStore::new(dir.path().join("sessions")));
    let provider = MockProvider::new()
        .then_response(tool_call_response(
            "c1",
            "bash",
            json!({"command": "curl -m 3 -s https://example.com >/dev/null 2>&1; echo net:$?"}),
        ))
        .then_response(tool_call_response(
            "c2",
            "bash",
            json!({"command": "echo inside > w.txt"}),
        ))
        .then_response(text_response("ran under sandbox"));
    let engine = engine_with(provider, Arc::clone(&store), dir.path(), |cfg| {
        cfg.approvals = ApprovalPolicy {
            mode: ApprovalMode::ApproveAll,
            non_interactive: true,
            allow: Vec::new(),
            deny: Vec::new(),
        };
        cfg.sandbox = SandboxPolicy::Seatbelt;
    });

    let summary = engine.run_task("probe the sandbox").await.unwrap();
    assert_eq!(summary.answer, "ran under sandbox");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("w.txt")).unwrap(),
        "inside\n"
    );
    let events = store.load(summary.session_id).await.unwrap();
    let results: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::ToolResult { outcome, .. } => Some(outcome.content.as_str()),
            _ => None,
        })
        .collect();
    assert!(results[0].contains("net:"), "probe ran: {}", results[0]);
    assert!(
        !results[0].contains("net:0"),
        "network must be denied under seatbelt: {}",
        results[0]
    );
    assert!(!results
        .iter()
        .any(|r| r.contains("Operation not permitted") && r.contains("w.txt")));
}

/// H1: a vetoing pre_tool hook keeps the file write from happening.
#[tokio::test]
async fn pre_tool_hook_vetoes_the_tool() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn SessionStore> =
        Arc::new(JsonlSessionStore::new(dir.path().join("sessions")));
    let provider = MockProvider::new()
        .then_response(tool_call_response(
            "c1",
            "write",
            json!({"path": "x.txt", "content": "hi"}),
        ))
        .then_response(text_response("heard the veto"));
    let engine = engine_with(provider, Arc::clone(&store), dir.path(), |cfg| {
        cfg.approvals = ApprovalPolicy {
            mode: ApprovalMode::ApproveAll,
            non_interactive: true,
            allow: Vec::new(),
            deny: Vec::new(),
        };
        cfg.hooks = HookConfig {
            pre_tool: Some("echo no-writes-allowed >&2; exit 1".to_string()),
            post_tool: None,
        };
    });

    let summary = engine.run_task("try to write").await.unwrap();
    assert!(
        !dir.path().join("x.txt").exists(),
        "the hook must prevent the write"
    );
    let events = store.load(summary.session_id).await.unwrap();
    let blocked = events
        .iter()
        .find_map(|e| match e {
            SessionEvent::ToolResult { outcome, .. } => Some(outcome.content.clone()),
            _ => None,
        })
        .expect("tool result");
    assert!(
        blocked.contains("blocked by pre_tool hook"),
        "got: {blocked}"
    );
    assert!(
        blocked.contains("no-writes-allowed"),
        "hook stderr surfaced: {blocked}"
    );
}

/// C1+C2: a checkpoint precedes the write; rewind restores the prior file
/// and the resume honors the truncation point.
#[tokio::test]
async fn checkpoint_then_rewind_restores_file_and_truncates_transcript() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("cfg.txt"), b"original").unwrap();
    let store: Arc<dyn SessionStore> =
        Arc::new(JsonlSessionStore::new(dir.path().join("sessions")));
    // The read-before-edit gate (Phase 13) requires the file to be read
    // before it may be overwritten — the script reads it first.
    let provider = MockProvider::new()
        .then_response(tool_call_response("c0", "read", json!({"path": "cfg.txt"})))
        .then_response(tool_call_response(
            "c1",
            "write",
            json!({"path": "cfg.txt", "content": "mutated", "overwrite": true}),
        ))
        .then_response(text_response("mutated the file"));
    let engine = engine_with(provider, Arc::clone(&store), dir.path(), |cfg| {
        cfg.approvals = ApprovalPolicy {
            mode: ApprovalMode::ApproveAll,
            non_interactive: true,
            allow: Vec::new(),
            deny: Vec::new(),
        };
    });

    let summary = engine.run_task("mutate cfg.txt").await.unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("cfg.txt")).unwrap(),
        "mutated"
    );

    // Rewind: the prior content comes back.
    let outcome =
        modron_core::rewind::rewind_last_checkpoint(store.as_ref(), summary.session_id, dir.path())
            .await
            .unwrap();
    assert_eq!(outcome.restored_path, "cfg.txt");
    assert!(!outcome.deleted);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("cfg.txt")).unwrap(),
        "original"
    );

    // Resume with the truncation point: abandoned events never re-enter
    // context — the model does NOT see the mutation turn again.
    let mut providers = HashMap::new();
    providers.insert(
        "mock".to_string(),
        Arc::new(MockProvider::new().then_response(text_response("resumed cleanly")))
            as Arc<dyn ModelProvider>,
    );
    let resumed_engine = Engine {
        selector: Arc::new(StaticSelector::new("mock-model", "mock")),
        providers,
        tools: Arc::new(default_registry()),
        store: Arc::clone(&store),
        config: EngineConfig {
            working_dir: dir.path().to_path_buf(),
            approvals: ApprovalPolicy {
                mode: ApprovalMode::ApproveAll,
                non_interactive: true,
                allow: Vec::new(),
                deny: Vec::new(),
            },
            resume_from_event: Some(outcome.truncated_at_event),
            ..EngineConfig::default()
        },
    };
    let resumed = resumed_engine
        .run_task_resuming(summary.session_id, "continue")
        .await
        .unwrap();
    assert_eq!(resumed.answer, "resumed cleanly");
}

/// C1: files the agent CREATED are deleted on rewind.
#[tokio::test]
async fn rewind_deletes_files_the_agent_created() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn SessionStore> =
        Arc::new(JsonlSessionStore::new(dir.path().join("sessions")));
    let provider = MockProvider::new()
        .then_response(tool_call_response(
            "c1",
            "write",
            json!({"path": "new.txt", "content": "hi"}),
        ))
        .then_response(text_response("created it"));
    let engine = engine_with(provider, Arc::clone(&store), dir.path(), |cfg| {
        cfg.approvals = ApprovalPolicy {
            mode: ApprovalMode::ApproveAll,
            non_interactive: true,
            allow: Vec::new(),
            deny: Vec::new(),
        };
    });
    let summary = engine.run_task("create a file").await.unwrap();
    assert!(dir.path().join("new.txt").exists());

    let outcome =
        modron_core::rewind::rewind_last_checkpoint(store.as_ref(), summary.session_id, dir.path())
            .await
            .unwrap();
    assert!(outcome.deleted);
    assert!(!dir.path().join("new.txt").exists());
}

/// Z1: the `zai` provider factory is actionable without a key.
#[test]
fn zai_factory_without_key_names_the_env_var() {
    let text = match provider_from_env("zai") {
        Err(err) => err.to_string(),
        Ok(_) => panic!("zai must require an API key"),
    };
    assert!(
        text.contains("ZAI_API_KEY"),
        "error must name the env var: {text}"
    );
}
