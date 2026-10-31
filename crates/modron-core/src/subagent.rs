//! Explore subagents (Phase 4): an RLM-style read-only child agent that
//! answers a question about the codebase under a hard token budget and
//! returns a brief — not raw tool dumps. Context stays small in the parent
//! (the brief); the child burns its own budget on the search.
//!
//! Safety/shape: the child runs ONLY read-only tools (read/glob/grep), on
//! the cheap tier (the parent selector's Search routing), with its own
//! session in the store and `explore` excluded from its registry (no
//! recursive spawning). Deltas/tool events are suppressed — the parent's
//! `● explore …` line is the only UI surface.

use crate::{
    ActionKind, Engine, EngineConfig, Error, ModelChoice, ModelSelector, Result, RunSummary,
    SelectRequest, ToolRegistry,
};
use std::sync::Arc;

/// Read-only tools the child may use. `explore` is deliberately absent.
pub const EXPLORE_TOOLS: [&str; 3] = ["read", "glob", "grep"];

/// A read-only selector that forces the cheap tier: it asks the parent
/// selector what Search would route to, then pins that choice. Budget and
/// overflow nuances are acceptable to lose here — explore is best-effort.
#[derive(Clone, Debug)]
pub struct ExploreSelector {
    choice: ModelChoice,
}

impl ExploreSelector {
    pub fn from_parent(parent: &Engine, task: &str) -> Self {
        let choice = parent.selector.select(&SelectRequest {
            task: task.to_string(),
            action: ActionKind::Search,
            context_tokens: 1_000,
            budget_remaining_usd: None,
        });
        Self { choice }
    }
}

impl ModelSelector for ExploreSelector {
    fn select(&self, _req: &SelectRequest) -> ModelChoice {
        ModelChoice {
            model: self.choice.model.clone(),
            provider: self.choice.provider.clone(),
            reason: format!(
                "explore: pinned cheap tier (parent: {})",
                self.choice.reason
            ),
            confident: true,
        }
    }
}

/// A selector that pins one (model, provider) pair on top of another
/// selector (persona-forced routing, Phase 5c A2).
#[derive(Clone)]
pub struct ForcedSelector {
    inner: Arc<dyn ModelSelector>,
    model: String,
    provider: String,
}

impl std::fmt::Debug for ForcedSelector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForcedSelector")
            .field("model", &self.model)
            .finish()
    }
}

impl ForcedSelector {
    pub fn new(
        inner: Arc<dyn ModelSelector>,
        model: impl Into<String>,
        provider: impl Into<String>,
    ) -> Self {
        Self {
            inner,
            model: model.into(),
            provider: provider.into(),
        }
    }
}

impl ModelSelector for ForcedSelector {
    fn select(&self, req: &SelectRequest) -> ModelChoice {
        let mut choice = self.inner.select(req);
        choice.model = self.model.clone();
        choice.provider = self.provider.clone();
        choice.reason = format!("persona: forced model {} — {}", self.model, choice.reason);
        choice.confident = true;
        choice
    }
}

#[derive(Clone, Debug)]
pub struct ExploreReport {
    pub brief: String,
    pub usage: crate::Usage,
    pub turns: u32,
}

/// Run one explore subagent (read-only, cheap tier, no persona).
pub async fn explore(parent: &Engine, task: &str, budget_tokens: u64) -> Result<ExploreReport> {
    delegate(
        parent,
        task,
        None,
        Some(&EXPLORE_TOOLS.map(str::to_string)),
        budget_tokens,
    )
    .await
}

/// Run a delegated subagent (Phase 5c A2): isolated context, its own budget
/// and session, optionally shaped by a named persona (system prompt, forced
/// model, approval lists) and a tool filter. Without a persona it inherits
/// the parent's selector and full registry — the orchestrator pattern's
/// "subtask in an isolated context". `delegate` itself is always excluded
/// from the child registry: no recursive spawning.
pub async fn delegate(
    parent: &Engine,
    task: &str,
    persona: Option<&crate::PersonaSpec>,
    tool_filter: Option<&[String]>,
    budget_tokens: u64,
) -> Result<ExploreReport> {
    if task.trim().is_empty() {
        return Err(Error::Config(
            "empty delegate task".into(),
            "pass a task".into(),
        ));
    }

    let mut registry = ToolRegistry::new();
    for spec in parent.tools.specs() {
        if spec.name == "delegate" {
            continue;
        }
        let allowed = match tool_filter {
            Some(filter) => filter.iter().any(|f| f == &spec.name),
            None => true,
        };
        if allowed {
            if let Some(tool) = parent.tools.get(&spec.name) {
                registry.register(Arc::clone(tool));
            }
        }
    }

    let selector: Arc<dyn ModelSelector> = match persona {
        Some(p) if p.model.is_some() => Arc::new(ForcedSelector::new(
            Arc::clone(&parent.selector),
            p.model.clone().unwrap_or_default(),
            p.provider.clone().unwrap_or_default(),
        )),
        _ => Arc::clone(&parent.selector),
    };

    let config = EngineConfig {
        working_dir: parent.config.working_dir.clone(),
        system: persona.and_then(|p| p.system.clone()).unwrap_or_default(),
        default_model: String::new(),
        default_provider: String::new(),
        max_turns: 24,
        max_input_tokens: parent.config.max_input_tokens,
        max_total_tokens: budget_tokens,
        approvals: crate::ApprovalPolicy {
            mode: parent.config.approvals.mode,
            non_interactive: parent.config.approvals.non_interactive,
            allow: persona.map(|p| p.allow.clone()).unwrap_or_default(),
            deny: persona.map(|p| p.deny.clone()).unwrap_or_default(),
        },
        ..EngineConfig::default()
    };
    let child = Engine {
        selector,
        providers: parent.providers.clone(),
        tools: Arc::new(registry),
        store: Arc::clone(&parent.store),
        config,
    };
    let RunSummary {
        answer,
        usage,
        turns,
        ..
    } = child.run_task(task).await?;
    Ok(ExploreReport {
        brief: answer,
        usage,
        turns,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{InMemoryStore, MockProvider, StaticSelector};
    use crate::Usage;
    use std::collections::HashMap;

    fn parent_engine(script: crate::testing::MockProvider) -> Engine {
        let mut providers: HashMap<String, Arc<dyn crate::ModelProvider>> = HashMap::new();
        providers.insert("anthropic".into(), Arc::new(script));
        Engine {
            selector: Arc::new(StaticSelector::new("claude-haiku-4-5", "anthropic")),
            providers,
            tools: Arc::new(ToolRegistry::new()),
            store: Arc::new(InMemoryStore::default()),
            config: EngineConfig {
                working_dir: std::env::temp_dir(),
                default_provider: "anthropic".into(),
                default_model: "claude-haiku-4-5".into(),
                ..EngineConfig::default()
            },
        }
    }

    #[tokio::test]
    async fn explore_runs_a_child_and_returns_the_brief() {
        use crate::{StopReason, TurnResponse};
        // First turn of the child: the final answer (no tools scripted).
        let answer = TurnResponse {
            text: "the token counter lives in lib.rs".into(),
            tool_calls: Vec::new(),
            usage: Usage {
                input_tokens: 900,
                output_tokens: 40,
                cached_input_tokens: 0,
            },
            stop_reason: StopReason::EndTurn,
        };
        let parent = parent_engine(MockProvider::new().then_response(answer));
        let report = explore(&parent, "where is the token counter?", 10_000)
            .await
            .unwrap();
        assert!(report.brief.contains("token counter"));
        assert_eq!(report.usage.total(), 940);
    }

    #[tokio::test]
    async fn child_budget_is_enforced() {
        use crate::{StopReason, ToolCall, TurnResponse};
        // Script that would run forever if unbudgeted: always another tool
        // call. The child must stop on its token budget (max_total_tokens).
        let tool_call = TurnResponse {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "c".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path": "x"}),
            }],
            usage: Usage {
                input_tokens: 5_000,
                output_tokens: 100,
                cached_input_tokens: 0,
            },
            stop_reason: StopReason::ToolUse,
        };
        let parent = parent_engine(
            MockProvider::new()
                .then_repeat_last()
                .then_response(tool_call),
        );
        let report = explore(&parent, "keep reading", 8_000).await.unwrap();
        assert!(
            report.usage.total() <= 8_000 + 6_000,
            "budget respected: {}",
            report.usage.total()
        );
    }

    #[test]
    fn explore_selector_pins_the_cheap_tier() {
        let parent = parent_engine(MockProvider::new());
        let selector = ExploreSelector::from_parent(&parent, "list files");
        let choice = selector.select(&SelectRequest {
            task: "ignored".into(),
            action: ActionKind::Plan,
            context_tokens: 1,
            budget_remaining_usd: None,
        });
        assert_eq!(
            choice.model, "claude-haiku-4-5",
            "pinned to the Search-tier model"
        );
    }
}
