//! Approval-policy acceptance tests (LEDGER A1). The gate is name-based on
//! the mutating tool class (`bash`, `write`, `edit`), so stub tools with
//! those names exercise the real engine path. Offline: MockProvider +
//! InMemoryStore, no filesystem mutation by the tools themselves.

use async_trait::async_trait;
use modron_core::testing::{InMemoryStore, MockProvider, StaticSelector};
use modron_core::{
    ApprovalMode, ApprovalPolicy, Engine, EngineConfig, Message, ModelProvider, Result, Role,
    SessionEvent, StopReason, Tool, ToolCall, ToolCtx, ToolOutcome, ToolRegistry, ToolSpec,
    TurnResponse, Usage,
};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// A stub named `write`: would mutate, records how often it actually ran.
struct WriteTool(AtomicUsize);

#[async_trait]
impl Tool for WriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write".into(),
            description: "stub write".into(),
            parameters: json!({"type": "object"}),
        }
    }
    async fn execute(&self, _args: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutcome> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutcome::ok("wrote"))
    }
}

/// A stub named `read`: read-only, never gated.
struct ReadTool(AtomicUsize);

#[async_trait]
impl Tool for ReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read".into(),
            description: "stub read".into(),
            parameters: json!({"type": "object"}),
        }
    }
    async fn execute(&self, _args: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutcome> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutcome::ok("contents"))
    }
}

fn registry() -> (Arc<ToolRegistry>, Arc<WriteTool>, Arc<ReadTool>) {
    let write = Arc::new(WriteTool(AtomicUsize::new(0)));
    let read = Arc::new(ReadTool(AtomicUsize::new(0)));
    let mut registry = ToolRegistry::new();
    registry.register(Arc::clone(&write) as Arc<dyn Tool>);
    registry.register(Arc::clone(&read) as Arc<dyn Tool>);
    (Arc::new(registry), write, read)
}

fn engine(
    provider: Arc<dyn ModelProvider>,
    approvals: ApprovalPolicy,
    tools: Arc<ToolRegistry>,
) -> Engine {
    let mut providers = HashMap::new();
    providers.insert("mock".to_string(), provider);
    Engine {
        selector: Arc::new(StaticSelector::new("mock-model", "mock")),
        providers,
        tools,
        store: Arc::new(InMemoryStore::new()),
        config: EngineConfig {
            working_dir: PathBuf::from("/tmp/modron-approval-test"),
            approvals,
            ..EngineConfig::default()
        },
    }
}

fn call_response(id: &str, name: &str) -> TurnResponse {
    TurnResponse {
        text: String::new(),
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: json!({}),
        }],
        usage: Usage::default(),
        stop_reason: StopReason::ToolUse,
    }
}

/// Every mode on a mutating tool: Deny and Ask block without executing (Ask
/// names `--approve-all`), ApproveAll executes.
#[tokio::test]
async fn mutating_tool_gated_per_mode() {
    for (mode, expect_execution, expect_hint) in [
        (ApprovalMode::Deny, false, false),
        (ApprovalMode::Ask, false, true),
        (ApprovalMode::ApproveAll, true, false),
    ] {
        let provider = Arc::new(
            MockProvider::new()
                .then_response(call_response("c1", "write"))
                .then_text("done"),
        );
        let (tools, write, _read) = registry();
        let engine = engine(
            provider.clone(),
            ApprovalPolicy {
                mode,
                non_interactive: true,
                allow: Vec::new(),
                deny: Vec::new(),
            },
            tools,
        );
        let summary = engine.run_task("write a file").await.unwrap();

        assert_eq!(
            write.0.load(Ordering::SeqCst),
            usize::from(expect_execution),
            "mode {mode:?}: execution count"
        );
        assert_eq!(
            summary.turns, 2,
            "mode {mode:?}: the model still gets a second turn"
        );

        // The block reaches the model as a tool error it can adapt to.
        let tool_message = &provider.requests()[1].messages[2];
        assert_eq!(tool_message.role, Role::Tool);
        if expect_execution {
            assert_eq!(tool_message.content, "wrote");
        } else {
            assert!(
                tool_message.content.contains("blocked by approval mode"),
                "{tool_message:?}"
            );
            assert_eq!(
                expect_hint,
                tool_message.content.contains("--approve-all"),
                "mode {mode:?}: hint presence"
            );
        }
    }
}

/// Read-only tools are never gated, in any mode.
#[tokio::test]
async fn read_only_tool_unaffected_by_every_mode() {
    for mode in [
        ApprovalMode::Deny,
        ApprovalMode::Ask,
        ApprovalMode::ApproveAll,
    ] {
        let provider = Arc::new(
            MockProvider::new()
                .then_response(call_response("c1", "read"))
                .then_text("done"),
        );
        let (tools, _write, read) = registry();
        let engine = engine(
            provider.clone(),
            ApprovalPolicy {
                mode,
                non_interactive: true,
                allow: Vec::new(),
                deny: Vec::new(),
            },
            tools,
        );
        let summary = engine.run_task("read a file").await.unwrap();
        assert_eq!(summary.answer, "done", "mode {mode:?}");
        assert_eq!(
            read.0.load(Ordering::SeqCst),
            1,
            "mode {mode:?}: read executed"
        );
    }
}

/// Model-adapts-after-block: blocked once, the model's next move is
/// read-only, which succeeds — the run completes instead of dying.
#[tokio::test]
async fn model_adapts_after_block() {
    let provider = Arc::new(
        MockProvider::new()
            .then_response(call_response("c1", "write"))
            .then_response(call_response("c2", "read"))
            .then_text("wrote nothing; here is what I found"),
    );
    let (tools, write, read) = registry();
    let engine = engine(
        provider.clone(),
        ApprovalPolicy {
            mode: ApprovalMode::Ask,
            non_interactive: true,
            allow: Vec::new(),
            deny: Vec::new(),
        },
        tools,
    );
    let summary = engine.run_task("update the file").await.unwrap();

    assert_eq!(summary.answer, "wrote nothing; here is what I found");
    assert_eq!(summary.turns, 3);
    assert_eq!(write.0.load(Ordering::SeqCst), 0, "write never executed");
    assert_eq!(
        read.0.load(Ordering::SeqCst),
        1,
        "model fell back to a read-only tool"
    );

    // The transcript shows block then the adapted call.
    let requests = provider.requests();
    assert!(requests[1].messages[2]
        .content
        .contains("blocked by approval mode"));
    assert_eq!(requests[2].messages[3].tool_calls[0].name, "read");
}

/// Approval gating is a run property, not persisted state: a blocked run's
/// session resumes fine and, with ApproveAll set on the resuming engine, the
/// same tool executes.
#[tokio::test]
async fn resume_unaffected_by_blocks() {
    let provider = Arc::new(
        MockProvider::new()
            .then_response(call_response("c1", "write"))
            .then_text("stopped"),
    );
    let (tools, write, _read) = registry();
    let engine = engine(
        provider,
        ApprovalPolicy {
            mode: ApprovalMode::Deny,
            non_interactive: true,
            allow: Vec::new(),
            deny: Vec::new(),
        },
        Arc::clone(&tools),
    );
    let blocked = engine.run_task("try to write").await.unwrap();
    assert_eq!(write.0.load(Ordering::SeqCst), 0);
    let events = engine.store.load(blocked.session_id).await.unwrap();
    assert!(events.iter().any(|event| matches!(
        event,
        SessionEvent::ToolResult { outcome, .. } if outcome.content.contains("blocked by approval mode")
    )));

    // Resume the SAME session with approval granted.
    let provider2 = Arc::new(MockProvider::new().then_text("resumed ok"));
    let mut providers: HashMap<String, Arc<dyn ModelProvider>> = HashMap::new();
    providers.insert("mock".to_string(), provider2.clone());
    let engine2 = Engine {
        selector: Arc::new(StaticSelector::new("mock-model", "mock")),
        providers,
        tools,
        store: Arc::clone(&engine.store),
        config: EngineConfig {
            working_dir: PathBuf::from("/tmp/modron-approval-test"),
            approvals: ApprovalPolicy {
                mode: ApprovalMode::ApproveAll,
                non_interactive: true,
                allow: Vec::new(),
                deny: Vec::new(),
            },
            ..EngineConfig::default()
        },
    };
    let summary = engine2
        .run_task_resuming(blocked.session_id, "go again")
        .await
        .unwrap();
    assert_eq!(summary.answer, "resumed ok");
    let resumed_first = &provider2.requests()[0].messages[0];
    assert_eq!(resumed_first.role, Role::User);
    assert_eq!(resumed_first.content, "try to write");
    let _ = Message::user("shape check");
}
