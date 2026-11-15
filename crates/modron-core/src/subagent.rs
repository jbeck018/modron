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
    SelectRequest, SessionEvent, SessionId, SessionMeta, SessionStore, ToolRegistry,
};
use async_trait::async_trait;
use std::sync::Arc;

/// Live meter for one child run (H3a): what the child has spent so far,
/// read from the child's own session events. Turns are the highest
/// `TurnTrace.turn` the child wrote (the counter the finish path reports as
/// `turns`); tokens are the sum of the child's `Usage` events, which the
/// engine pairs one-to-one with every `add_usage` into the run total, so the
/// sum equals the finish path's `usage.total()` (input + output, saturating).
#[derive(Default)]
struct ChildMeter {
    state: std::sync::Mutex<MeterState>,
}

#[derive(Default)]
struct MeterState {
    session: Option<SessionId>,
    turns: u32,
    tokens: u64,
}

impl ChildMeter {
    fn snapshot(&self) -> (u32, u64) {
        let s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        (s.turns, s.tokens)
    }
}

/// Store adapter for one child run: delegates every call to the parent's
/// store and counts the child's own usage events as they are appended. Only
/// the session this adapter's engine creates is counted, so sibling
/// subagents and the parent's own events never leak into the meter.
struct MeteredStore {
    inner: Arc<dyn SessionStore>,
    meter: Arc<ChildMeter>,
}

impl MeteredStore {
    fn observe(&self, id: SessionId, events: &[SessionEvent]) {
        let mut s = self.meter.state.lock().unwrap_or_else(|p| p.into_inner());
        if s.session != Some(id) {
            return;
        }
        for event in events {
            match event {
                SessionEvent::Usage { usage, .. } => {
                    s.tokens = s.tokens.saturating_add(usage.total());
                }
                SessionEvent::TurnTrace { turn, .. } => s.turns = s.turns.max(*turn),
                _ => {}
            }
        }
    }
}

#[async_trait]
impl SessionStore for MeteredStore {
    async fn create_session(&self, title: &str) -> Result<SessionMeta> {
        let meta = self.inner.create_session(title).await?;
        self.meter
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .session = Some(meta.id);
        Ok(meta)
    }

    async fn append(&self, id: SessionId, event: &SessionEvent) -> Result<()> {
        self.inner.append(id, event).await?;
        self.observe(id, std::slice::from_ref(event));
        Ok(())
    }

    async fn append_batch(&self, id: SessionId, events: &[SessionEvent]) -> Result<()> {
        self.inner.append_batch(id, events).await?;
        self.observe(id, events);
        Ok(())
    }

    async fn load(&self, id: SessionId) -> Result<Vec<SessionEvent>> {
        self.inner.load(id).await
    }

    async fn list(&self) -> Result<Vec<SessionMeta>> {
        self.inner.list().await
    }

    async fn delete(&self, id: SessionId) -> Result<bool> {
        self.inner.delete(id).await
    }

    async fn fork(&self, from: SessionId) -> Result<SessionId> {
        self.inner.fork(from).await
    }

    async fn load_snapshot_locked(
        &self,
        id: SessionId,
    ) -> Result<(tokio::sync::OwnedMutexGuard<()>, Vec<SessionEvent>)> {
        self.inner.load_snapshot_locked(id).await
    }

    async fn event_count(&self, id: SessionId) -> Result<u64> {
        self.inner.event_count(id).await
    }
}

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
            context_window: self.choice.context_window,
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
        None,
    )
    .await
}

/// Run a delegated subagent (Phase 5c A2): isolated context, its own budget
/// and session, optionally shaped by a named persona (system prompt, forced
/// model, approval lists) and a tool filter. Without a persona it inherits
/// the parent's selector and full registry — the orchestrator pattern's
/// "subtask in an isolated context". `delegate` itself is always excluded
/// from the child registry: no recursive spawning. `working_dir` overrides
/// the parent's working dir (worktree isolation passes the worktree here —
/// without it the "isolated" child would mutate the main tree).
pub async fn delegate(
    parent: &Engine,
    task: &str,
    persona: Option<&crate::PersonaSpec>,
    tool_filter: Option<&[String]>,
    budget_tokens: u64,
    working_dir: Option<&std::path::Path>,
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
                // Carry the parent's essential/discoverable tiering: a
                // tool the parent stubs stays a stub in the child, so the
                // child's prompt prefix keeps the same small-schema shape
                // (tool_search remains available for the full schema).
                if parent.tools.is_on_demand(&spec.name) {
                    registry.mark_on_demand(&spec.name);
                }
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

    // Subagent identity + parent event plumbing (Phase 5h): the child's
    // tool activity is re-emitted through the parent's sink as tagged
    // heartbeats, and start/finish events bracket the run.
    let subagent_id = uuid::Uuid::new_v4().to_string();
    let label = if persona.is_some() {
        "persona".to_string()
    } else {
        "delegate".to_string()
    };
    let parent_sink = parent.config.on_tool_event.clone();
    if let Some(sink) = &parent_sink {
        sink(crate::ToolActivity::SubagentStarted {
            id: subagent_id.clone(),
            label: label.clone(),
        });
    }
    let meter = Arc::new(ChildMeter::default());
    let progress_id = subagent_id.clone();
    let progress_label = label.clone();
    let progress_meter = Arc::clone(&meter);
    let progress_sink: Option<Arc<dyn Fn(crate::ToolActivity) + Send + Sync>> =
        parent_sink.map(|parent| {
            let sink: Arc<dyn Fn(crate::ToolActivity) + Send + Sync> =
                Arc::new(move |activity: crate::ToolActivity| {
                    let heartbeat = match &activity {
                        crate::ToolActivity::Done { name, .. } => {
                            let (turns, tokens) = progress_meter.snapshot();
                            crate::ToolActivity::SubagentProgress {
                                id: progress_id.clone(),
                                label: progress_label.clone(),
                                turns,
                                tokens,
                                last_tool: Some(name.clone()),
                            }
                        }
                        other => other.clone(),
                    };
                    parent(heartbeat);
                });
            sink
        });

    let config = child_config(
        &parent.config,
        persona,
        working_dir,
        budget_tokens,
        progress_sink,
    );
    let child = Engine {
        selector,
        providers: parent.providers.clone(),
        tools: Arc::new(registry),
        store: Arc::new(MeteredStore {
            inner: Arc::clone(&parent.store),
            meter,
        }),
        config,
    };
    let run = child.run_task(task).await;
    if let Some(sink) = parent.config.on_tool_event.as_ref() {
        let (tokens, turns, is_error) = match &run {
            Ok(summary) => (summary.usage.total(), summary.turns, false),
            Err(_) => (0, 0, true),
        };
        sink(crate::ToolActivity::SubagentFinished {
            id: subagent_id.clone(),
            label: label.clone(),
            tokens,
            turns,
            is_error,
        });
    }
    let RunSummary {
        answer,
        usage,
        turns,
        ..
    } = run?;
    Ok(ExploreReport {
        brief: answer,
        usage,
        turns,
    })
}

/// Build the child `EngineConfig` for a subagent (D2: inherit, never widen).
/// The child copies the parent's sandbox, hooks, cost cap and price lookup,
/// and its deny list is the UNION of the parent's and the persona's. The
/// allow list is the persona's only (never inherited from the parent).
pub(crate) fn child_config(
    parent: &EngineConfig,
    persona: Option<&crate::PersonaSpec>,
    working_dir: Option<&std::path::Path>,
    budget_tokens: u64,
    on_tool_event: Option<Arc<crate::ToolActivitySink>>,
) -> EngineConfig {
    let mut deny = parent.approvals.deny.clone();
    if let Some(p) = persona {
        for entry in &p.deny {
            if !deny.contains(entry) {
                deny.push(entry.clone());
            }
        }
    }
    EngineConfig {
        working_dir: working_dir
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| parent.working_dir.clone()),
        system: persona.and_then(|p| p.system.clone()).unwrap_or_default(),
        default_model: String::new(),
        default_provider: String::new(),
        max_turns: 24,
        max_input_tokens: parent.max_input_tokens,
        max_total_tokens: budget_tokens,
        approvals: crate::ApprovalPolicy {
            mode: parent.approvals.mode,
            non_interactive: parent.approvals.non_interactive,
            allow: persona.map(|p| p.allow.clone()).unwrap_or_default(),
            deny,
            // Rules are inherited unchanged: a child can never escape a
            // parent `ask` or `deny` rule by running as a subagent.
            rules: parent.approvals.rules.clone(),
        },
        sandbox: parent.sandbox,
        hooks: parent.hooks.clone(),
        max_cost_usd: parent.max_cost_usd,
        price_lookup: parent.price_lookup.clone(),
        on_tool_event,
        ..EngineConfig::default()
    }
}

/// Short unique id for worktrees and background subagents.
pub fn new_worktree_id() -> String {
    uuid::Uuid::new_v4().to_string()[..8].to_string()
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

    fn strict_parent_config() -> EngineConfig {
        use crate::hooks::HookConfig;
        use crate::{ApprovalMode, ApprovalPolicy, SandboxPolicy};
        EngineConfig {
            working_dir: std::path::PathBuf::from("/parent/worktree"),
            sandbox: SandboxPolicy::Bubblewrap,
            approvals: ApprovalPolicy {
                mode: ApprovalMode::default(),
                non_interactive: false,
                allow: vec!["diagnostics".into()],
                deny: vec!["bash".into()],
                rules: Default::default(),
            },
            hooks: HookConfig {
                pre_tool: Some("true".into()),
                post_tool: None,
            },
            max_cost_usd: Some(1.0),
            price_lookup: Some(Arc::new(|_: &str| Some((1.0, 1.0)))),
            ..EngineConfig::default()
        }
    }

    #[test]
    fn child_config_inherits_sandbox_hooks_and_cost_cap() {
        let parent = strict_parent_config();
        let child = child_config(&parent, None, None, 5_000, None);
        assert_eq!(child.sandbox, crate::SandboxPolicy::Bubblewrap);
        assert_eq!(child.hooks, parent.hooks);
        assert_eq!(child.max_cost_usd, Some(1.0));
        let parent_lookup = parent.price_lookup.as_ref().expect("parent lookup");
        let child_lookup = child.price_lookup.as_ref().expect("child lookup inherited");
        assert!(Arc::ptr_eq(parent_lookup, child_lookup));
        assert_eq!(child.max_total_tokens, 5_000);
        assert_eq!(child.working_dir, parent.working_dir);
    }

    #[test]
    fn child_config_unions_parent_and_persona_deny_lists() {
        let parent = strict_parent_config();
        let persona = crate::PersonaSpec {
            system: Some("be terse".into()),
            model: None,
            provider: None,
            allow: vec!["read".into()],
            deny: vec!["write".into(), "bash".into()],
        };
        let child = child_config(&parent, Some(&persona), None, 1_000, None);
        assert!(child.approvals.deny.contains(&"bash".to_string()));
        assert!(child.approvals.deny.contains(&"write".to_string()));
        assert_eq!(
            child.approvals.deny.iter().filter(|d| *d == "bash").count(),
            1,
            "shared deny entries are not duplicated"
        );
        assert_eq!(child.approvals.allow, vec!["read".to_string()]);
        assert_eq!(child.system, "be terse");
    }

    #[tokio::test]
    async fn progress_heartbeat_carries_live_turns_and_tokens() {
        use crate::{
            StopReason, ToolActivity, ToolCall, ToolCtx, ToolOutcome, ToolSpec, TurnResponse,
        };
        use std::sync::Mutex;

        // A read-only tool the child can call; the parent registers it and
        // the explore-style filter hands it to the child.
        struct ReadStub;
        #[async_trait]
        impl crate::Tool for ReadStub {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: "read".into(),
                    description: "stub".into(),
                    parameters: serde_json::json!({"type": "object"}),
                }
            }
            async fn execute(
                &self,
                _args: serde_json::Value,
                _ctx: &ToolCtx,
            ) -> Result<ToolOutcome> {
                Ok(ToolOutcome::ok("file body"))
            }
        }

        // Turn 1 calls the tool (1000 in + 50 out); turn 2 answers (600 + 30).
        let tool_turn = TurnResponse {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path": "x"}),
            }],
            usage: Usage {
                input_tokens: 1_000,
                output_tokens: 50,
                cached_input_tokens: 0,
            },
            stop_reason: StopReason::ToolUse,
        };
        let answer_turn = TurnResponse {
            text: "found it".into(),
            tool_calls: Vec::new(),
            usage: Usage {
                input_tokens: 600,
                output_tokens: 30,
                cached_input_tokens: 0,
            },
            stop_reason: StopReason::EndTurn,
        };
        let mut parent = parent_engine(
            MockProvider::new()
                .then_response(tool_turn)
                .then_response(answer_turn),
        );
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(ReadStub));
        parent.tools = Arc::new(registry);

        let seen: Arc<Mutex<Vec<ToolActivity>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_seen = Arc::clone(&seen);
        let sink: Arc<crate::ToolActivitySink> = Arc::new(move |activity: ToolActivity| {
            sink_seen.lock().unwrap().push(activity);
        });
        parent.config.on_tool_event = Some(sink);

        let filter = ["read".to_string()];
        let report = delegate(&parent, "find it", None, Some(&filter), 100_000, None)
            .await
            .unwrap();
        assert_eq!(report.turns, 2);
        assert_eq!(report.usage.total(), 1_680);

        let events = seen.lock().unwrap().clone();
        let heartbeats: Vec<(u32, u64)> = events
            .iter()
            .filter_map(|e| match e {
                ToolActivity::SubagentProgress { turns, tokens, .. } => Some((*turns, *tokens)),
                _ => None,
            })
            .collect();
        assert!(
            !heartbeats.is_empty(),
            "a tool completion emits a heartbeat"
        );
        let (live_turns, live_tokens) = heartbeats[0];
        assert!(live_turns >= 1, "heartbeat reports the turn so far");
        assert!(live_tokens > 0, "heartbeat reports tokens so far");
        assert_eq!((live_turns, live_tokens), (1, 1_050));

        let (last_turns, last_tokens) = *heartbeats.last().unwrap();
        let (finished_turns, finished_tokens) = events
            .iter()
            .find_map(|e| match e {
                ToolActivity::SubagentFinished { tokens, turns, .. } => Some((*turns, *tokens)),
                _ => None,
            })
            .expect("finished event");
        assert!(finished_turns >= last_turns);
        assert!(finished_tokens >= last_tokens);
    }

    #[test]
    fn child_config_without_persona_keeps_parent_deny_and_clears_allow() {
        let parent = strict_parent_config();
        let child = child_config(&parent, None, None, 1_000, None);
        assert_eq!(child.approvals.deny, vec!["bash".to_string()]);
        assert!(child.approvals.allow.is_empty());
    }
}
