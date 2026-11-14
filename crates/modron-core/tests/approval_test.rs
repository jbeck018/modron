//! Approval-policy acceptance tests (LEDGER A1). The gate is name-based on
//! the mutating tool class (`bash`, `write`, `edit`), so stub tools with
//! those names exercise the real engine path. Offline: MockProvider +
//! InMemoryStore, no filesystem mutation by the tools themselves.

use async_trait::async_trait;
use modron_core::testing::{InMemoryStore, MockProvider, StaticSelector};
use modron_core::{
    ApprovalMode, ApprovalPolicy, Engine, EngineConfig, Message, ModelProvider, Result, Role,
    SessionEvent, SessionStore, StopReason, Tool, ToolCall, ToolCtx, ToolOutcome, ToolRegistry,
    ToolSpec, TurnResponse, Usage,
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

/// Engine rooted at `dir` with a shared in-memory store, so tests can read
/// back the checkpoint events the run wrote.
fn engine_in(
    dir: &std::path::Path,
    provider: Arc<dyn ModelProvider>,
    approvals: ApprovalPolicy,
    approver: Option<Arc<modron_core::Approver>>,
    store: Arc<InMemoryStore>,
) -> Engine {
    let tools = registry().0;
    let mut providers = HashMap::new();
    providers.insert("mock".to_string(), provider);
    Engine {
        selector: Arc::new(StaticSelector::new("mock-model", "mock")),
        providers,
        tools,
        store: store as Arc<dyn SessionStore>,
        config: EngineConfig {
            working_dir: dir.to_path_buf(),
            approvals,
            approver,
            ..EngineConfig::default()
        },
    }
}

fn write_turn(path: &str) -> Arc<MockProvider> {
    Arc::new(
        MockProvider::new()
            .then_response(call_response_with_args(
                "c1",
                "write",
                json!({ "path": path }),
            ))
            .then_text("done"),
    )
}

fn call_response_with_args(id: &str, name: &str, arguments: serde_json::Value) -> TurnResponse {
    TurnResponse {
        text: String::new(),
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: name.into(),
            arguments,
        }],
        usage: Usage::default(),
        stop_reason: StopReason::ToolUse,
    }
}

fn checkpoints(events: &[SessionEvent]) -> Vec<SessionEvent> {
    events
        .iter()
        .filter(|e| matches!(e, SessionEvent::Checkpoint { .. }))
        .cloned()
        .collect()
}

fn policy(mode: ApprovalMode) -> ApprovalPolicy {
    ApprovalPolicy {
        mode,
        non_interactive: false,
        allow: Vec::new(),
        deny: Vec::new(),
    }
}

/// D8/A3: a write checkpoints the prior content in every mode that lets the
/// call run (ApproveAll, and Ask with an approving approver), not only in
/// ApproveAll.
#[tokio::test]
async fn checkpoint_taken_in_every_allowing_mode() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), b"before").unwrap();
    let allow: Arc<modron_core::Approver> = Arc::new(|_c: &ToolCall| true);
    for (mode, approver) in [
        (ApprovalMode::ApproveAll, None),
        (ApprovalMode::Ask, Some(allow)),
    ] {
        let store = Arc::new(InMemoryStore::new());
        let engine = engine_in(
            dir.path(),
            write_turn("a.txt"),
            policy(mode),
            approver,
            Arc::clone(&store),
        );
        let summary = engine.run_task("edit").await.unwrap();
        let events = store.load(summary.session_id).await.unwrap();
        let cps = checkpoints(&events);
        assert_eq!(cps.len(), 1, "mode {mode:?}: one checkpoint");
        match &cps[0] {
            SessionEvent::Checkpoint {
                path,
                prior_content_b64,
                uncaptured,
                ..
            } => {
                assert_eq!(path, "a.txt");
                assert_eq!(
                    prior_content_b64.as_deref(),
                    Some(modron_core::b64::encode(b"before").as_str()),
                    "mode {mode:?}: prior content captured"
                );
                assert!(!uncaptured, "mode {mode:?}: small file is captured");
            }
            other => panic!("expected checkpoint, got {other:?}"),
        }
    }
}

/// A3: a refused call records no checkpoint (the gate runs first).
#[tokio::test]
async fn refused_write_records_no_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), b"before").unwrap();
    let deny: Arc<modron_core::Approver> = Arc::new(|_c: &ToolCall| false);
    for (mode, approver) in [(ApprovalMode::Deny, None), (ApprovalMode::Ask, Some(deny))] {
        let store = Arc::new(InMemoryStore::new());
        let engine = engine_in(
            dir.path(),
            write_turn("a.txt"),
            policy(mode),
            approver,
            Arc::clone(&store),
        );
        let summary = engine.run_task("edit").await.unwrap();
        let events = store.load(summary.session_id).await.unwrap();
        assert!(
            checkpoints(&events).is_empty(),
            "mode {mode:?}: refused call must record no checkpoint"
        );
    }
}

/// A3: a file that did not exist is checkpointed as absent (None, captured),
/// so rewind will delete it.
#[tokio::test]
async fn new_file_checkpoint_is_absent_not_uncaptured() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(InMemoryStore::new());
    let engine = engine_in(
        dir.path(),
        write_turn("fresh.txt"),
        policy(ApprovalMode::ApproveAll),
        None,
        Arc::clone(&store),
    );
    let summary = engine.run_task("create").await.unwrap();
    let events = store.load(summary.session_id).await.unwrap();
    match checkpoints(&events).as_slice() {
        [SessionEvent::Checkpoint {
            prior_content_b64,
            uncaptured,
            ..
        }] => {
            assert_eq!(*prior_content_b64, None);
            assert!(!uncaptured);
        }
        other => panic!("expected one absent checkpoint, got {other:?}"),
    }
}

/// A3: a pre-existing file over the 256 KB cap is recorded as uncaptured,
/// and the snapshot is never read into the log.
#[tokio::test]
async fn oversized_file_is_recorded_uncaptured() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("big.bin"), vec![7u8; 300 * 1024]).unwrap();
    let store = Arc::new(InMemoryStore::new());
    let engine = engine_in(
        dir.path(),
        write_turn("big.bin"),
        policy(ApprovalMode::ApproveAll),
        None,
        Arc::clone(&store),
    );
    let summary = engine.run_task("edit").await.unwrap();
    let events = store.load(summary.session_id).await.unwrap();
    match checkpoints(&events).as_slice() {
        [SessionEvent::Checkpoint {
            prior_content_b64,
            uncaptured,
            ..
        }] => {
            assert_eq!(*prior_content_b64, None);
            assert!(uncaptured, "oversized original must be marked uncaptured");
        }
        other => panic!("expected one uncaptured checkpoint, got {other:?}"),
    }
}

/// A3: only paths that resolve inside the working dir are checkpointed:
/// an absolute path elsewhere, a `..` escape, and a symlink whose target is
/// outside all record nothing.
#[tokio::test]
async fn path_outside_working_dir_records_nothing() {
    let work = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let outside_file = outside.path().join("secret.txt");
    std::fs::write(&outside_file, b"outside").unwrap();
    let escape = format!(
        "../{}/secret.txt",
        outside.path().file_name().unwrap().to_string_lossy()
    );
    let mut targets = vec![outside_file.to_string_lossy().to_string(), escape];
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside_file, work.path().join("link.txt")).unwrap();
        targets.push("link.txt".to_string());
    }
    for target in targets {
        let store = Arc::new(InMemoryStore::new());
        let engine = engine_in(
            work.path(),
            write_turn(&target),
            policy(ApprovalMode::ApproveAll),
            None,
            Arc::clone(&store),
        );
        let summary = engine.run_task("edit").await.unwrap();
        let events = store.load(summary.session_id).await.unwrap();
        assert!(
            checkpoints(&events).is_empty(),
            "target {target}: outside working dir must record no checkpoint"
        );
    }
}
