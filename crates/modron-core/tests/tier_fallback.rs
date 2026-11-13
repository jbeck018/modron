//! Tier fallback tests (remote-control hardening): an auth-class provider
//! failure on a tier's primary promotes the run to the tier's fallback
//! model — once, then for the rest of the run. Non-auth failures and
//! unconfigured fallbacks surface unchanged.

use async_trait::async_trait;
use modron_core::testing::{InMemoryStore, MockProvider};
use modron_core::{
    Engine, EngineConfig, Error, ModelProvider, Result, StopReason, TierFallbacks, Tool, ToolCall,
    ToolCtx, ToolOutcome, ToolRegistry, ToolSpec, TurnRequest, TurnResponse, Usage,
};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

/// A provider whose every turn fails with an auth-class error (counts
/// its calls so tests can assert the dead primary was hit exactly once).
struct AuthFail {
    id: &'static str,
    calls: AtomicU32,
}

#[async_trait]
impl ModelProvider for AuthFail {
    fn id(&self) -> &str {
        self.id
    }

    async fn turn(&self, _req: std::sync::Arc<TurnRequest>) -> Result<TurnResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(Error::Provider {
            provider: self.id.to_string(),
            message: "HTTP 401 Unauthorized: {\"error\":\"OAuth access token has been revoked.\"}"
                .into(),
        })
    }
}

/// A provider whose every turn fails with a NON-auth error.
struct ServerFail {
    id: &'static str,
}

#[async_trait]
impl ModelProvider for ServerFail {
    fn id(&self) -> &str {
        self.id
    }

    async fn turn(&self, _req: std::sync::Arc<TurnRequest>) -> Result<TurnResponse> {
        Err(Error::Provider {
            provider: self.id.to_string(),
            message: "HTTP 500 Internal Server Error: overloaded".into(),
        })
    }
}

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

fn scripted_mock(answer: &str) -> Arc<MockProvider> {
    let response = TurnResponse {
        text: answer.into(),
        tool_calls: vec![],
        usage: Usage {
            input_tokens: 10,
            output_tokens: 5,
            cached_input_tokens: 0,
        },
        stop_reason: StopReason::EndTurn,
    };
    Arc::new(MockProvider::with_script(vec![response]))
}

fn engine(
    primary: Arc<dyn ModelProvider>,
    fallback: Option<Arc<MockProvider>>,
    fallbacks: Option<TierFallbacks>,
) -> Engine {
    let mut providers: HashMap<String, Arc<dyn ModelProvider>> = HashMap::new();
    providers.insert("primary".to_string(), primary);
    if let Some(fb) = fallback {
        providers.insert("fallback".to_string(), fb);
    }
    Engine {
        selector: Arc::new(modron_core::testing::StaticSelector::new(
            "m-strong", "primary",
        )),
        providers,
        tools: Arc::new({
            let mut registry = ToolRegistry::new();
            registry.register(Arc::new(EchoTool));
            registry
        }),
        store: Arc::new(InMemoryStore::new()),
        config: EngineConfig {
            working_dir: PathBuf::from("/tmp/modron-tier-fallback-test"),
            tier_fallbacks: fallbacks.map(Arc::new),
            ..EngineConfig::default()
        },
    }
}

fn strong_fallbacks() -> TierFallbacks {
    TierFallbacks {
        cheap: vec![],
        strong: vec![
            ("m-strong".into(), "primary".into()),
            ("m-strong-fallback".into(), "fallback".into()),
        ],
    }
}

#[tokio::test]
async fn auth_failure_falls_back_and_completes() {
    let primary = Arc::new(AuthFail {
        id: "primary",
        calls: AtomicU32::new(0),
    });
    let engine = engine(
        primary,
        Some(scripted_mock("fallback answer")),
        Some(strong_fallbacks()),
    );
    let summary = engine.run_task("hello").await.unwrap();
    assert_eq!(summary.answer, "fallback answer");
    assert_eq!(summary.models_used, ["m-strong-fallback"]);
}

#[tokio::test]
async fn fallback_persists_for_the_rest_of_the_run() {
    // The fallback's script: turn 1 calls the tool, turn 2 answers. The
    // dead primary must be hit EXACTLY once (the override keeps the
    // fallback selected for turn 2 — it must not 401 again per turn).
    let response_tool = TurnResponse {
        text: String::new(),
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            name: "echo_tool".into(),
            arguments: json!({}),
        }],
        usage: Usage::default(),
        stop_reason: StopReason::ToolUse,
    };
    let response_final = TurnResponse {
        text: "done after the tool".into(),
        tool_calls: vec![],
        usage: Usage {
            input_tokens: 20,
            output_tokens: 5,
            cached_input_tokens: 0,
        },
        stop_reason: StopReason::EndTurn,
    };
    let engine = engine(
        Arc::new(AuthFail {
            id: "primary",
            calls: AtomicU32::new(0),
        }),
        Some(Arc::new(MockProvider::with_script(vec![
            response_tool,
            response_final,
        ]))),
        Some(strong_fallbacks()),
    );
    let summary = engine.run_task("hello").await.unwrap();
    assert_eq!(summary.answer, "done after the tool");
    assert_eq!(summary.models_used, ["m-strong-fallback"]);
    assert_eq!(summary.turns, 2);
}

#[tokio::test]
async fn non_auth_failure_does_not_fall_back() {
    let engine = engine(
        Arc::new(ServerFail { id: "primary" }),
        Some(scripted_mock("should never run")),
        Some(strong_fallbacks()),
    );
    let err = engine.run_task("hello").await.unwrap_err();
    assert!(err.to_string().contains("500"), "{err}");
}

#[tokio::test]
async fn unconfigured_fallbacks_surface_the_auth_error() {
    let engine = engine(
        Arc::new(AuthFail {
            id: "primary",
            calls: AtomicU32::new(0),
        }),
        Some(scripted_mock("should never run")),
        None,
    );
    let err = engine.run_task("hello").await.unwrap_err();
    assert!(err.to_string().contains("401"), "{err}");
}
