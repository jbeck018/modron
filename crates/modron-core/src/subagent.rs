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
    /// The parent's price lookup, so the child's spend is priced per model
    /// (team mode caps spend across its children with it).
    price: Option<Arc<crate::PriceLookup>>,
}

#[derive(Default)]
struct MeterState {
    session: Option<SessionId>,
    turns: u32,
    /// Every usage event the child wrote, summed (its `total()` is the
    /// token meter the heartbeat reports).
    usage: crate::Usage,
    /// The child's priced spend in USD (0 with no price lookup).
    spend_usd: f64,
}

impl ChildMeter {
    fn snapshot(&self) -> (u32, u64) {
        let s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        (s.turns, s.usage.total())
    }

    /// The child's usage so far (a killed child reports what it spent).
    fn usage(&self) -> crate::Usage {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).usage
    }

    /// The child's priced spend so far.
    fn spend_usd(&self) -> f64 {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .spend_usd
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
                SessionEvent::Usage { model, usage } => {
                    s.usage.input_tokens = s.usage.input_tokens.saturating_add(usage.input_tokens);
                    s.usage.output_tokens =
                        s.usage.output_tokens.saturating_add(usage.output_tokens);
                    s.usage.cached_input_tokens = s
                        .usage
                        .cached_input_tokens
                        .saturating_add(usage.cached_input_tokens);
                    if let Some(lookup) = self.meter.price.as_ref() {
                        if let Some((in_per_m, out_per_m)) = lookup(model) {
                            s.spend_usd += usage.input_tokens as f64 / 1_000_000.0 * in_per_m
                                + usage.output_tokens as f64 / 1_000_000.0 * out_per_m;
                        }
                    }
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
    /// Reason-string label: `persona`, `pin` or `role` (audit trail).
    label: &'static str,
}

impl std::fmt::Debug for ForcedSelector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForcedSelector")
            .field("model", &self.model)
            .field("label", &self.label)
            .finish()
    }
}

impl ForcedSelector {
    /// A persona's forced model (the default label). Pins routing.
    pub fn new(
        inner: Arc<dyn ModelSelector>,
        model: impl Into<String>,
        provider: impl Into<String>,
    ) -> Self {
        Self::labeled(inner, model, provider, "persona")
    }

    /// An explicit pin (`--provider`): pins routing like a persona does.
    pub fn pin(
        inner: Arc<dyn ModelSelector>,
        model: impl Into<String>,
        provider: impl Into<String>,
    ) -> Self {
        Self::labeled(inner, model, provider, "pin")
    }

    /// A configured [`crate::ModelRoles`] model forced onto a child. Does
    /// not pin: the reason string says `role`.
    pub fn role(
        inner: Arc<dyn ModelSelector>,
        model: impl Into<String>,
        provider: impl Into<String>,
    ) -> Self {
        Self::labeled(inner, model, provider, "role")
    }

    fn labeled(
        inner: Arc<dyn ModelSelector>,
        model: impl Into<String>,
        provider: impl Into<String>,
        label: &'static str,
    ) -> Self {
        Self {
            inner,
            model: model.into(),
            provider: provider.into(),
            label,
        }
    }
}

impl ModelSelector for ForcedSelector {
    fn select(&self, req: &SelectRequest) -> ModelChoice {
        let mut choice = self.inner.select(req);
        choice.model = self.model.clone();
        choice.provider = self.provider.clone();
        // A forced model's window is not the inner choice's window.
        if self.label != "persona" {
            choice.context_window = None;
        }
        choice.reason = format!(
            "{}: forced model {} — {}",
            self.label, self.model, choice.reason
        );
        choice.confident = true;
        choice
    }

    fn pinned(&self) -> bool {
        self.label != "role"
    }
}

#[derive(Clone, Debug)]
pub struct ExploreReport {
    pub brief: String,
    pub usage: crate::Usage,
    pub turns: u32,
    /// H3d: a user stopped the child with `kill_agent`. `brief` then says so,
    /// and `usage` / `turns` are what it spent before the stop (the parent
    /// still charges them).
    pub killed: bool,
    /// Priced spend of the child in USD (0 without a price lookup). Team
    /// mode sums it against the spend cap.
    pub spend_usd: f64,
}

/// Run one explore subagent (read-only, cheap tier, no persona).
pub async fn explore(parent: &Engine, task: &str, budget_tokens: u64) -> Result<ExploreReport> {
    // `explore` opts out of the `subagent` role (H7c): it stays on the router.
    delegate_owned_inner(
        ChildOwner::default(),
        parent,
        task,
        None,
        Some(&EXPLORE_TOOLS.map(str::to_string)),
        budget_tokens,
        None,
        false,
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
    delegate_owned(
        ChildOwner::default(),
        parent,
        task,
        persona,
        tool_filter,
        budget_tokens,
        working_dir,
    )
    .await
}

/// Who a child belongs to (H3c, H3d). `session` is the session the child is
/// addressed under by [`steer_agent`] and [`kill_agent`]. `token` is the run
/// of that session that is live when the child is spawned: the child's asks
/// reach the parent approver only while that run lasts (`None` refuses every
/// ask). `background` marks a detached child, which pushes
/// `ToolActivity::BackgroundFinished` when it settles.
#[derive(Clone, Copy, Debug, Default)]
pub struct ChildOwner {
    pub session: Option<SessionId>,
    pub token: Option<crate::RunToken>,
    pub background: bool,
}

impl ChildOwner {
    /// A foreground child of `session`, approval-scoped to that session's
    /// live run (if any). Call it during the spawning tool call: a background
    /// child must capture the run before it is spawned.
    pub fn for_session(session: Option<SessionId>) -> Self {
        Self {
            session,
            token: session.and_then(|s| crate::run_scope().token(s)),
            background: false,
        }
    }

    /// The same child, detached (see [`ChildOwner::background`]).
    pub fn detached(self) -> Self {
        Self {
            background: true,
            ..self
        }
    }
}

/// Characters of a settled child's brief kept in `BackgroundFinished`.
const FINISHED_BRIEF_CHARS: usize = 200;

/// How a child's run ended: its own result, or a user kill (H3d).
enum Ended {
    Finished(Result<crate::RunSummary>),
    Killed,
}

/// Live children, keyed by swarm-row id (the child's `subagent_id`). The
/// registry is process-wide, like the run scope: the server clones one
/// template config into every session, so an engine-held map would be
/// shared, and a process-wide map keyed by session keeps lookups free of
/// engine locks. An entry lives exactly as long as its child run.
struct LiveChild {
    session: SessionId,
    steer: Arc<crate::SteerInbox>,
    /// Taken by the first kill; sending it ends the child's run.
    kill: Option<tokio::sync::oneshot::Sender<()>>,
}

static LIVE_CHILDREN: std::sync::Mutex<std::collections::BTreeMap<String, LiveChild>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

fn live_children() -> std::sync::MutexGuard<'static, std::collections::BTreeMap<String, LiveChild>>
{
    LIVE_CHILDREN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Removes its child from the registry when dropped, so every exit path
/// (normal end, error, kill, task abort) leaves no stale entry behind.
struct ChildGuard(String);

impl ChildGuard {
    fn register(
        id: String,
        session: SessionId,
        steer: Arc<crate::SteerInbox>,
        kill: tokio::sync::oneshot::Sender<()>,
    ) -> Self {
        live_children().insert(
            id.clone(),
            LiveChild {
                session,
                steer,
                kill: Some(kill),
            },
        );
        Self(id)
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        live_children().remove(&self.0);
    }
}

fn no_live_agent(agent: &str) -> String {
    format!("no live sub-agent `{agent}` in this session")
}

/// H3d: queue a steer message into a live child's inbox. The child's run
/// loop drains it at its next safe boundary, so its next model turn carries
/// it. Errors when `agent` is not a live child of `session`.
pub fn steer_agent(
    session: SessionId,
    agent: &str,
    text: String,
) -> std::result::Result<(), String> {
    let children = live_children();
    match children.get(agent).filter(|child| child.session == session) {
        Some(child) => {
            child.steer.push(text);
            Ok(())
        }
        None => Err(no_live_agent(agent)),
    }
}

/// H3d: stop one live child. Only that child's run ends (its parent run and
/// siblings continue); its tool call or `delegate_status` entry reports it as
/// stopped. Errors when `agent` is not a live child of `session`, or when a
/// stop is already under way.
pub fn kill_agent(session: SessionId, agent: &str) -> std::result::Result<(), String> {
    let mut children = live_children();
    let child = children
        .get_mut(agent)
        .filter(|child| child.session == session)
        .ok_or_else(|| no_live_agent(agent))?;
    match child.kill.take() {
        Some(kill) => {
            // The receiver lives in the child's own task; a send only fails
            // when that task already ended, which the guard prevents.
            let _ = kill.send(());
            Ok(())
        }
        None => Err(format!("sub-agent `{agent}` is already being stopped")),
    }
}

/// H3d: the ids of `session`'s live children (swarm-row ids).
pub fn live_agents(session: SessionId) -> Vec<String> {
    live_children()
        .iter()
        .filter(|(_, child)| child.session == session)
        .map(|(id, _)| id.clone())
        .collect()
}

/// H3c: [`delegate`] for a child that belongs to a known parent run. `owner`
/// carries the run whose asks it may make (see [`ChildOwner`]); with no run
/// every ask the child makes is refused. A detached owner reports the child
/// through `BackgroundFinished` when it settles.
pub async fn delegate_owned(
    owner: ChildOwner,
    parent: &Engine,
    task: &str,
    persona: Option<&crate::PersonaSpec>,
    tool_filter: Option<&[String]>,
    budget_tokens: u64,
    working_dir: Option<&std::path::Path>,
) -> Result<ExploreReport> {
    delegate_owned_inner(
        owner,
        parent,
        task,
        persona,
        tool_filter,
        budget_tokens,
        working_dir,
        true,
    )
    .await
}

/// [`delegate_owned`] with the `subagent` model role switchable off
/// (`subagent_role = false` for `explore`, H7c).
#[allow(clippy::too_many_arguments)]
async fn delegate_owned_inner(
    owner: ChildOwner,
    parent: &Engine,
    task: &str,
    persona: Option<&crate::PersonaSpec>,
    tool_filter: Option<&[String]>,
    budget_tokens: u64,
    working_dir: Option<&std::path::Path>,
    subagent_role: bool,
) -> Result<ExploreReport> {
    let subagent_id = uuid::Uuid::new_v4().to_string();
    let label = if persona.is_some() {
        "persona".to_string()
    } else {
        "delegate".to_string()
    };
    let outcome: Result<ExploreReport> = async {
        if task.trim().is_empty() {
            return Err(Error::Config(
                "empty delegate task".into(),
                "pass a task".into(),
            ));
        }
        run_child(
            &owner,
            &subagent_id,
            &label,
            parent,
            task,
            persona,
            tool_filter,
            budget_tokens,
            working_dir,
            subagent_role,
        )
        .await
    }
    .await;
    if owner.background {
        if let Some(sink) = parent.config.on_tool_event.as_ref() {
            let (ok, brief) = match &outcome {
                Ok(report) => (!report.killed, report.brief.clone()),
                Err(e) => (false, e.to_string()),
            };
            sink(crate::ToolActivity::BackgroundFinished {
                id: subagent_id.clone(),
                label: label.clone(),
                ok,
                brief: brief.chars().take(FINISHED_BRIEF_CHARS).collect(),
            });
        }
    }
    outcome
}

/// The child run behind [`delegate_owned`] (its id and label already fixed).
#[allow(clippy::too_many_arguments)]
async fn run_child(
    owner: &ChildOwner,
    subagent_id: &str,
    label: &str,
    parent: &Engine,
    task: &str,
    persona: Option<&crate::PersonaSpec>,
    tool_filter: Option<&[String]>,
    budget_tokens: u64,
    working_dir: Option<&std::path::Path>,
    subagent_role: bool,
) -> Result<ExploreReport> {
    let subagent_id = subagent_id.to_string();
    let label = label.to_string();
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

    // Precedence (H7c): a persona's forced model > the `subagent` role >
    // the parent's routing. The role is resolved like the escalation turn:
    // the parent's selector provider, the role's model id, and a warning
    // plus the parent's routing when it does not resolve.
    let selector: Arc<dyn ModelSelector> = match persona {
        Some(p) if p.model.is_some() => Arc::new(ForcedSelector::new(
            Arc::clone(&parent.selector),
            p.model.clone().unwrap_or_default(),
            p.provider.clone().unwrap_or_default(),
        )),
        _ => match parent
            .config
            .roles
            .subagent
            .as_deref()
            .filter(|_| subagent_role && !parent.selector.pinned())
        {
            Some(model) => {
                let base = parent.selector.select(&SelectRequest {
                    task: task.to_string(),
                    action: ActionKind::Chat,
                    context_tokens: crate::estimate_tokens(task),
                    budget_remaining_usd: None,
                });
                // The child carries the role's resolved provider and wire
                // model (a `provider::model` prefix is stripped), exactly as
                // a persona's forced provider is passed.
                match parent.role_choice(base, model, "subagent") {
                    Some(choice) => Arc::new(ForcedSelector::role(
                        Arc::clone(&parent.selector),
                        choice.model,
                        choice.provider,
                    )),
                    None => Arc::clone(&parent.selector),
                }
            }
            None => Arc::clone(&parent.selector),
        },
    };

    // Subagent identity + parent event plumbing (Phase 5h): the child's
    // tool activity is re-emitted through the parent's sink as tagged
    // heartbeats, and start/finish events bracket the run.
    let parent_sink = parent.config.on_tool_event.clone();
    if let Some(sink) = &parent_sink {
        sink(crate::ToolActivity::SubagentStarted {
            id: subagent_id.clone(),
            label: label.clone(),
        });
    }
    let meter = Arc::new(ChildMeter {
        price: parent.config.price_lookup.clone(),
        ..ChildMeter::default()
    });
    let progress_id = subagent_id.clone();
    let progress_label = label.clone();
    let progress_meter = Arc::clone(&meter);
    let progress_sink: Option<Arc<dyn Fn(crate::ToolActivity) + Send + Sync>> =
        parent_sink.clone().map(|parent| {
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

    let mut config = child_config(
        &parent.config,
        persona,
        working_dir,
        budget_tokens,
        progress_sink,
    );
    // H3b/H3c: the child asks through the parent's approver, tagged with the
    // child's label, and only while the owning run is live. H3d: each ask is
    // bracketed by `WaitingApproval` / `Running` state events.
    config.approver = with_approval_state(
        child_approver(parent.config.approver.as_ref(), owner.token, &label),
        parent_sink.clone(),
        subagent_id.clone(),
        label.clone(),
    );
    // H3d: the child's own steer inbox (the parent's is never inherited).
    // `steer_agent` pushes into it; the run loop drains it between turns.
    let steer = Arc::new(crate::SteerInbox::new());
    config.steer = Some(Arc::clone(&steer));
    let child = Engine {
        selector,
        providers: parent.providers.clone(),
        tools: Arc::new(registry),
        store: Arc::new(MeteredStore {
            inner: Arc::clone(&parent.store),
            meter: Arc::clone(&meter),
        }),
        config,
    };
    // H3d: an owned child is addressable under its session until it ends.
    // The kill sender lives in the registry entry, so the receiver only
    // fires on a real kill; an unowned child has no kill arm.
    let (kill_tx, mut kill_rx) = tokio::sync::oneshot::channel::<()>();
    let guard = owner
        .session
        .map(|session| ChildGuard::register(subagent_id.clone(), session, steer, kill_tx));
    let ended = tokio::select! {
        result = child.run_task(task) => Ended::Finished(result),
        _ = &mut kill_rx, if guard.is_some() => Ended::Killed,
    };
    let run = match ended {
        Ended::Finished(run) => run,
        Ended::Killed => {
            // A stop: the child's spend so far still counts for the parent.
            let (turns, _) = meter.snapshot();
            let usage = meter.usage();
            if let Some(sink) = &parent_sink {
                sink(crate::ToolActivity::SubagentState {
                    id: subagent_id.clone(),
                    label: label.clone(),
                    state: crate::AgentState::Killed,
                });
                sink(crate::ToolActivity::SubagentFinished {
                    id: subagent_id.clone(),
                    label: label.clone(),
                    tokens: usage.total(),
                    turns,
                    is_error: true,
                });
            }
            drop(guard);
            return Ok(ExploreReport {
                brief: format!("sub-agent {label} was stopped by the user"),
                usage,
                turns,
                killed: true,
                spend_usd: meter.spend_usd(),
            });
        }
    };
    if let Some(sink) = parent_sink.as_ref() {
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
        killed: false,
        spend_usd: meter.spend_usd(),
    })
}

/// H3d: wrap a child's approver so each ask is bracketed by a
/// `WaitingApproval` state (before) and `Running` (after) event through the
/// parent's sink. A child with no approver keeps `None`.
fn with_approval_state(
    inner: Option<Arc<crate::Approver>>,
    sink: Option<Arc<crate::ToolActivitySink>>,
    id: String,
    label: String,
) -> Option<Arc<crate::Approver>> {
    let inner = inner?;
    let Some(sink) = sink else {
        return Some(inner);
    };
    Some(Arc::new(move |ask: &crate::ApprovalAsk<'_>| {
        sink(crate::ToolActivity::SubagentState {
            id: id.clone(),
            label: label.clone(),
            state: crate::AgentState::WaitingApproval,
        });
        let allowed = inner(ask);
        sink(crate::ToolActivity::SubagentState {
            id: id.clone(),
            label: label.clone(),
            state: crate::AgentState::Running,
        });
        allowed
    }))
}

/// H3c: the approver a child asks through. An ask is forwarded to the
/// parent's approver only while the parent run in `token` is still live;
/// otherwise it is refused without prompting anyone. Enforced here, in core,
/// so every transport (TTY, TUI, server) gets the same rule.
pub(crate) fn child_approver(
    parent: Option<&Arc<crate::Approver>>,
    token: Option<crate::RunToken>,
    label: &str,
) -> Option<Arc<crate::Approver>> {
    let parent = Arc::clone(parent?);
    let agent = label.to_string();
    Some(Arc::new(move |ask: &crate::ApprovalAsk<'_>| {
        let live = token.is_some_and(|t| crate::run_scope().is_live(t));
        if !live {
            tracing::info!(
                agent = agent.as_str(),
                tool = ask.call.name.as_str(),
                "subagent approval refused: parent run ended; approval unavailable"
            );
            return false;
        }
        parent(&crate::ApprovalAsk {
            call: ask.call,
            agent: Some(agent.as_str()),
        })
    }))
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
        provider_lookup: parent.provider_lookup.clone(),
        on_tool_event,
        // Sub-agents inherit the parent's reasoning effort (H7a); no
        // delegate or persona argument overrides it yet.
        effort: parent.effort,
        // Sub-agents inherit the reasoning-summary opt-in too.
        reasoning_summary: parent.reasoning_summary,
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
            provider_blocks: Vec::new(),
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
            provider_blocks: Vec::new(),
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

    /// H7a: a sub-agent inherits the parent's reasoning effort.
    #[test]
    fn child_inherits_the_parent_effort() {
        let mut parent = strict_parent_config();
        parent.effort = Some(crate::Effort::Low);
        let child = child_config(&parent, None, None, 1_000, None);
        assert_eq!(child.effort, Some(crate::Effort::Low));
        parent.effort = None;
        assert_eq!(child_config(&parent, None, None, 1_000, None).effort, None);
    }

    /// H7b: a sub-agent inherits the parent's reasoning-summary opt-in.
    #[test]
    fn child_inherits_the_parent_reasoning_summary() {
        let mut parent = strict_parent_config();
        parent.reasoning_summary = true;
        assert!(child_config(&parent, None, None, 1_000, None).reasoning_summary);
        parent.reasoning_summary = false;
        assert!(!child_config(&parent, None, None, 1_000, None).reasoning_summary);
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
                ..Default::default()
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
            provider_blocks: Vec::new(),
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
            provider_blocks: Vec::new(),
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

/// H3b: a child's gated calls ask the parent's approver, tagged with the
/// child's label; a child is never more permissive than its parent.
#[cfg(test)]
mod approval_tests {
    use super::*;
    use crate::testing::{InMemoryStore, MockProvider, StaticSelector};
    use crate::{ApprovalAsk, ApprovalMode, ApprovalPolicy, StopReason, Tool, ToolCall, ToolCtx};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// A gated `write` the child may use; counts the calls that really ran.
    struct WriteStub {
        runs: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for WriteStub {
        fn spec(&self) -> crate::ToolSpec {
            crate::ToolSpec {
                name: "write".into(),
                description: "stub".into(),
                parameters: serde_json::json!({"type": "object"}),
            }
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolCtx,
        ) -> Result<crate::ToolOutcome> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            Ok(crate::ToolOutcome::ok("wrote"))
        }
    }

    /// (tool name, asking agent) for every ask the parent's approver saw.
    type Asks = Arc<Mutex<Vec<(String, Option<String>)>>>;

    fn recording_approver(allow: bool) -> (Arc<crate::Approver>, Asks) {
        let asks: Asks = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&asks);
        let approver: Arc<crate::Approver> = Arc::new(move |ask: &ApprovalAsk<'_>| {
            log.lock()
                .unwrap()
                .push((ask.call.name.clone(), ask.agent.map(str::to_string)));
            allow
        });
        (approver, asks)
    }

    /// Child script: one `write` call, then a final answer.
    fn child_script() -> MockProvider {
        let write = crate::TurnResponse {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "w1".into(),
                name: "write".into(),
                arguments: serde_json::json!({"path": "x"}),
            }],
            usage: crate::Usage::default(),
            stop_reason: StopReason::ToolUse,
            provider_blocks: Vec::new(),
        };
        MockProvider::new()
            .then_response(write)
            .then_text("child done")
    }

    fn parent(
        mode: ApprovalMode,
        approver: Option<Arc<crate::Approver>>,
        runs: Arc<AtomicUsize>,
    ) -> Engine {
        let mut providers: HashMap<String, Arc<dyn crate::ModelProvider>> = HashMap::new();
        providers.insert("anthropic".into(), Arc::new(child_script()));
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(WriteStub { runs }));
        Engine {
            selector: Arc::new(StaticSelector::new("claude-haiku-4-5", "anthropic")),
            providers,
            tools: Arc::new(registry),
            store: Arc::new(InMemoryStore::default()),
            config: EngineConfig {
                working_dir: std::env::temp_dir(),
                default_provider: "anthropic".into(),
                default_model: "claude-haiku-4-5".into(),
                approvals: ApprovalPolicy {
                    mode,
                    ..ApprovalPolicy::default()
                },
                approver,
                ..EngineConfig::default()
            },
        }
    }

    /// Run one child inside a live parent run (the child's asks are honoured
    /// only while that run is live, so the run must span the call).
    async fn run_child(engine: &Engine) -> ExploreReport {
        let session = uuid::Uuid::new_v4();
        let _run = crate::run_scope().begin(session);
        let owner = ChildOwner::for_session(Some(session));
        let filter = ["write".to_string()];
        delegate_owned(
            owner,
            engine,
            "write the file",
            None,
            Some(&filter),
            100_000,
            None,
        )
        .await
        .unwrap()
    }

    fn write_call() -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: "write".into(),
            arguments: serde_json::json!({}),
        }
    }

    #[test]
    fn child_asks_follow_only_the_run_that_spawned_it() {
        let session = uuid::Uuid::new_v4();
        let (approver, asks) = recording_approver(true);
        let call = write_call();
        let ask = ApprovalAsk {
            call: &call,
            agent: None,
        };
        // Run 1 spawns the child.
        let run1 = crate::run_scope().begin(session);
        let child = child_approver(
            Some(&approver),
            crate::run_scope().token(session),
            "delegate",
        )
        .unwrap();
        assert!(child(&ask), "allowed during its own run");
        assert_eq!(asks.lock().unwrap().len(), 1);
        drop(run1);
        assert!(!child(&ask), "refused once run 1 has ended");
        // Run 2: its approver would say yes (allow-all), but the child is
        // still bound to run 1.
        let run2 = crate::run_scope().begin(session);
        assert!(
            !child(&ask),
            "refused during run 2 though the approver says yes"
        );
        drop(run2);
        assert_eq!(
            asks.lock().unwrap().len(),
            1,
            "the parent approver was consulted only during run 1"
        );
    }

    #[test]
    fn a_child_without_a_run_is_refused_and_other_sessions_never_match() {
        let (approver, asks) = recording_approver(true);
        let call = write_call();
        let ask = ApprovalAsk {
            call: &call,
            agent: None,
        };
        let session = uuid::Uuid::new_v4();
        let other = uuid::Uuid::new_v4();
        let _run = crate::run_scope().begin(session);
        let no_run = child_approver(Some(&approver), None, "delegate").unwrap();
        assert!(!no_run(&ask), "no run token, no prompt");
        let foreign = crate::run_scope().token(session);
        let other_run = crate::run_scope().begin(other);
        let child = child_approver(Some(&approver), foreign, "delegate").unwrap();
        assert!(child(&ask), "its own session's run is still live");
        drop(other_run);
        let token_other = crate::run_scope().token(other);
        assert!(token_other.is_none(), "a finished run leaves no token");
        assert!(asks.lock().unwrap().len() == 1);
    }

    /// Records the session id the probe tool was called in.
    struct Probe {
        seen: Arc<Mutex<Option<uuid::Uuid>>>,
    }

    #[async_trait]
    impl Tool for Probe {
        fn spec(&self) -> crate::ToolSpec {
            crate::ToolSpec {
                name: "probe".into(),
                description: "probe".into(),
                parameters: serde_json::json!({"type": "object"}),
            }
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            ctx: &ToolCtx,
        ) -> Result<crate::ToolOutcome> {
            *self.seen.lock().unwrap() = ctx.session_id;
            Ok(crate::ToolOutcome::ok("seen"))
        }
    }

    /// One probe tool call, then whatever the script does next.
    fn probe_engine(
        provider: Arc<dyn crate::ModelProvider>,
        seen: Arc<Mutex<Option<uuid::Uuid>>>,
    ) -> Engine {
        let mut providers: HashMap<String, Arc<dyn crate::ModelProvider>> = HashMap::new();
        providers.insert("anthropic".into(), provider);
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(Probe { seen }));
        Engine {
            selector: Arc::new(StaticSelector::new("claude-haiku-4-5", "anthropic")),
            providers,
            tools: Arc::new(registry),
            store: Arc::new(InMemoryStore::default()),
            config: EngineConfig {
                working_dir: std::env::temp_dir(),
                default_provider: "anthropic".into(),
                default_model: "claude-haiku-4-5".into(),
                ..EngineConfig::default()
            },
        }
    }

    /// Calls the probe once, then stalls forever (for the abort test).
    struct StallAfterFirst {
        first: MockProvider,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl crate::ModelProvider for StallAfterFirst {
        fn id(&self) -> &str {
            "mock"
        }
        async fn turn(&self, req: Arc<crate::TurnRequest>) -> Result<crate::TurnResponse> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                self.first.turn(req).await
            } else {
                std::future::pending().await
            }
        }
    }

    #[tokio::test]
    async fn the_run_scope_ends_on_an_error_exit() {
        let seen = Arc::new(Mutex::new(None));
        // The script runs out after the probe turn, so the run errors.
        let provider = MockProvider::new().then_tool_call("p1", "probe", serde_json::json!({}));
        let engine = probe_engine(Arc::new(provider), Arc::clone(&seen));
        assert!(engine.run_task("go").await.is_err());
        let session = seen.lock().unwrap().expect("the probe ran in the session");
        assert!(
            crate::run_scope().token(session).is_none(),
            "an error exit ends the run"
        );
    }

    #[tokio::test]
    async fn the_run_scope_ends_when_the_run_is_aborted() {
        let seen = Arc::new(Mutex::new(None));
        let provider = StallAfterFirst {
            first: MockProvider::new().then_tool_call("p1", "probe", serde_json::json!({})),
            calls: AtomicUsize::new(0),
        };
        let engine = Arc::new(probe_engine(Arc::new(provider), Arc::clone(&seen)));
        let task = {
            let engine = Arc::clone(&engine);
            tokio::spawn(async move { engine.run_task("go").await })
        };
        let mut session = None;
        for _ in 0..500 {
            session = *seen.lock().unwrap();
            if session.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let session = session.expect("the probe ran in the session");
        assert!(
            crate::run_scope().token(session).is_some(),
            "the run is live while it stalls"
        );
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(
            crate::run_scope().token(session).is_none(),
            "an aborted run ends its scope"
        );
    }

    #[tokio::test]
    async fn child_ask_carries_its_label_and_a_grant_runs_the_tool() {
        let runs = Arc::new(AtomicUsize::new(0));
        let (approver, asks) = recording_approver(true);
        let engine = parent(ApprovalMode::Ask, Some(approver), Arc::clone(&runs));
        let report = run_child(&engine).await;
        assert_eq!(report.brief, "child done");
        assert_eq!(
            *asks.lock().unwrap(),
            vec![("write".to_string(), Some("delegate".to_string()))],
            "the child's ask reaches the parent approver with its label"
        );
        assert_eq!(runs.load(Ordering::SeqCst), 1, "approved call ran");
    }

    #[tokio::test]
    async fn child_denial_refuses_the_gated_tool() {
        let runs = Arc::new(AtomicUsize::new(0));
        let (approver, asks) = recording_approver(false);
        let engine = parent(ApprovalMode::Ask, Some(approver), Arc::clone(&runs));
        run_child(&engine).await;
        assert_eq!(asks.lock().unwrap().len(), 1, "the child did ask");
        assert_eq!(runs.load(Ordering::SeqCst), 0, "denied call never ran");
    }

    #[tokio::test]
    async fn deny_mode_refuses_the_child_without_asking() {
        let runs = Arc::new(AtomicUsize::new(0));
        let (approver, asks) = recording_approver(true);
        let engine = parent(ApprovalMode::Deny, Some(approver), Arc::clone(&runs));
        run_child(&engine).await;
        assert!(
            asks.lock().unwrap().is_empty(),
            "deny never reaches the approver"
        );
        assert_eq!(runs.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn without_an_approver_the_child_stays_refused() {
        let runs = Arc::new(AtomicUsize::new(0));
        let engine = parent(ApprovalMode::Ask, None, Arc::clone(&runs));
        run_child(&engine).await;
        assert_eq!(
            runs.load(Ordering::SeqCst),
            0,
            "no approver, no prompt, no run"
        );
    }
}

/// H3d: live addressing (steer / kill), the child registry, the waiting-approval
/// state, and the background finish event.
#[cfg(test)]
mod h3d_tests {
    use super::*;
    use crate::testing::{InMemoryStore, MockProvider, StaticSelector};
    use crate::{
        AgentState, ApprovalAsk, ApprovalMode, Role, StopReason, Tool, ToolActivity, ToolCtx,
        ToolOutcome, ToolSpec, TurnResponse, Usage,
    };
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use tokio::sync::Notify;

    /// A `read` tool that announces it started and blocks until `release`.
    struct PauseTool {
        started: Arc<Notify>,
        release: Arc<Notify>,
    }

    #[async_trait]
    impl Tool for PauseTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "read".into(),
                description: "pauses until released".into(),
                parameters: serde_json::json!({"type": "object"}),
            }
        }
        async fn execute(&self, _args: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutcome> {
            self.started.notify_one();
            self.release.notified().await;
            Ok(ToolOutcome::ok("released"))
        }
    }

    fn pause_engine(
        provider: Arc<dyn crate::ModelProvider>,
        started: &Arc<Notify>,
        release: &Arc<Notify>,
    ) -> Engine {
        let mut providers: HashMap<String, Arc<dyn crate::ModelProvider>> = HashMap::new();
        providers.insert("anthropic".into(), provider);
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(PauseTool {
            started: Arc::clone(started),
            release: Arc::clone(release),
        }));
        Engine {
            selector: Arc::new(StaticSelector::new("claude-haiku-4-5", "anthropic")),
            providers,
            tools: Arc::new(registry),
            store: Arc::new(InMemoryStore::default()),
            config: EngineConfig {
                working_dir: std::env::temp_dir(),
                default_provider: "anthropic".into(),
                default_model: "claude-haiku-4-5".into(),
                ..EngineConfig::default()
            },
        }
    }

    /// Captures every tool-activity event the parent's sink receives.
    fn capture(engine: &mut Engine) -> Arc<Mutex<Vec<ToolActivity>>> {
        let events: Arc<Mutex<Vec<ToolActivity>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_events = Arc::clone(&events);
        let sink: Arc<crate::ToolActivitySink> = Arc::new(move |activity: ToolActivity| {
            sink_events.lock().unwrap().push(activity);
        });
        engine.config.on_tool_event = Some(sink);
        events
    }

    fn started_id(events: &Mutex<Vec<ToolActivity>>) -> Option<String> {
        events.lock().unwrap().iter().find_map(|e| match e {
            ToolActivity::SubagentStarted { id, .. } => Some(id.clone()),
            _ => None,
        })
    }

    fn state_events(events: &Mutex<Vec<ToolActivity>>) -> Vec<AgentState> {
        events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                ToolActivity::SubagentState { state, .. } => Some(*state),
                _ => None,
            })
            .collect()
    }

    fn read_filter() -> Vec<String> {
        vec!["read".to_string()]
    }

    /// A `read` call that spent 1000 in + 50 out before it blocks.
    fn spending_read_call() -> TurnResponse {
        TurnResponse {
            text: String::new(),
            tool_calls: vec![crate::ToolCall {
                id: "c1".into(),
                name: "read".into(),
                arguments: serde_json::json!({}),
            }],
            usage: Usage {
                input_tokens: 1_000,
                output_tokens: 50,
                cached_input_tokens: 0,
            },
            stop_reason: StopReason::ToolUse,
            provider_blocks: Vec::new(),
        }
    }

    #[tokio::test]
    async fn steering_a_running_child_reaches_its_next_turn() {
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let mock = Arc::new(
            MockProvider::new()
                .then_tool_call("c1", "read", serde_json::json!({}))
                .then_text("answer"),
        );
        let engine = Arc::new(pause_engine(mock.clone(), &started, &release));
        let session = uuid::Uuid::new_v4();
        let other = uuid::Uuid::new_v4();
        let owner = ChildOwner::for_session(Some(session));
        let task = {
            let engine = Arc::clone(&engine);
            tokio::spawn(async move {
                let filter = read_filter();
                delegate_owned(
                    owner,
                    &engine,
                    "find it",
                    None,
                    Some(&filter),
                    100_000,
                    None,
                )
                .await
            })
        };
        started.notified().await;
        let ids = live_agents(session);
        assert_eq!(ids.len(), 1, "the running child is addressable");
        assert!(
            steer_agent(other, &ids[0], "wrong session".into()).is_err(),
            "a steer names the child's own session"
        );
        steer_agent(session, &ids[0], "use the other crate".into()).unwrap();
        release.notify_one();
        task.await.unwrap().unwrap();

        let reqs = mock.requests();
        assert_eq!(reqs.len(), 2, "the steer did not add a turn of its own");
        assert!(
            reqs[1]
                .messages
                .iter()
                .any(|m| m.content == "use the other crate" && matches!(m.role, Role::User)),
            "the next turn carries the steer"
        );
    }

    #[tokio::test]
    async fn killing_a_foreground_child_returns_the_stop_and_counts_its_tokens() {
        let started = Arc::new(Notify::new());
        // Never released: the child stays parked until it is killed.
        let release = Arc::new(Notify::new());
        let mock = Arc::new(MockProvider::new().then_response(spending_read_call()));
        let mut engine = pause_engine(mock, &started, &release);
        let events = capture(&mut engine);
        let engine = Arc::new(engine);
        let session = uuid::Uuid::new_v4();
        let owner = ChildOwner::for_session(Some(session));
        let task = {
            let engine = Arc::clone(&engine);
            tokio::spawn(async move {
                let filter = read_filter();
                delegate_owned(
                    owner,
                    &engine,
                    "find it",
                    None,
                    Some(&filter),
                    100_000,
                    None,
                )
                .await
            })
        };
        started.notified().await;
        let ids = live_agents(session);
        assert_eq!(ids.len(), 1);
        kill_agent(session, &ids[0]).unwrap();
        let report = task.await.unwrap().unwrap();

        assert!(report.killed);
        assert!(
            report.brief.contains("was stopped by the user"),
            "{}",
            report.brief
        );
        assert_eq!(
            report.usage.total(),
            1_050,
            "tokens spent before the stop still count"
        );
        assert!(
            live_agents(session).is_empty(),
            "the registry entry is gone"
        );
        assert!(
            kill_agent(session, &ids[0]).is_err(),
            "a stopped child is gone"
        );
        assert_eq!(state_events(&events), vec![AgentState::Killed]);
    }

    /// A gated `write` the child calls; counts the calls that really ran.
    struct GatedWrite {
        runs: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl Tool for GatedWrite {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "write".into(),
                description: "gated write".into(),
                parameters: serde_json::json!({"type": "object"}),
            }
        }
        async fn execute(&self, _args: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutcome> {
            self.runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(ToolOutcome::ok("wrote"))
        }
    }

    #[tokio::test]
    async fn a_child_ask_is_waiting_approval_until_answered() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let runs = Arc::new(AtomicUsize::new(0));
        let events: Arc<Mutex<Vec<ToolActivity>>> = Arc::new(Mutex::new(Vec::new()));
        // The state the parent approver sees at the moment it is asked.
        let seen_at_ask: Arc<Mutex<Option<AgentState>>> = Arc::new(Mutex::new(None));
        let ask_events = Arc::clone(&events);
        let ask_seen = Arc::clone(&seen_at_ask);
        let approver: Arc<crate::Approver> = Arc::new(move |_ask: &ApprovalAsk<'_>| {
            let state = state_events(&ask_events).last().copied();
            *ask_seen.lock().unwrap() = state;
            true
        });
        let mut providers: HashMap<String, Arc<dyn crate::ModelProvider>> = HashMap::new();
        providers.insert(
            "anthropic".into(),
            Arc::new(
                MockProvider::new()
                    .then_tool_call("w1", "write", serde_json::json!({"path": "x"}))
                    .then_text("wrote it"),
            ),
        );
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(GatedWrite {
            runs: Arc::clone(&runs),
        }));
        let sink_events = Arc::clone(&events);
        let sink: Arc<crate::ToolActivitySink> = Arc::new(move |activity: ToolActivity| {
            sink_events.lock().unwrap().push(activity);
        });
        let engine = Engine {
            selector: Arc::new(StaticSelector::new("claude-haiku-4-5", "anthropic")),
            providers,
            tools: Arc::new(registry),
            store: Arc::new(InMemoryStore::default()),
            config: EngineConfig {
                working_dir: std::env::temp_dir(),
                default_provider: "anthropic".into(),
                default_model: "claude-haiku-4-5".into(),
                approvals: crate::ApprovalPolicy {
                    mode: ApprovalMode::Ask,
                    ..crate::ApprovalPolicy::default()
                },
                approver: Some(approver),
                on_tool_event: Some(sink),
                ..EngineConfig::default()
            },
        };
        // The child's asks reach the parent approver only inside a live run.
        let session = uuid::Uuid::new_v4();
        let _run = crate::run_scope().begin(session);
        let filter = ["write".to_string()];
        delegate_owned(
            ChildOwner::for_session(Some(session)),
            &engine,
            "write the file",
            None,
            Some(&filter),
            100_000,
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            *seen_at_ask.lock().unwrap(),
            Some(AgentState::WaitingApproval)
        );
        assert_eq!(
            state_events(&events),
            vec![AgentState::WaitingApproval, AgentState::Running],
            "the ask is bracketed by waiting and running"
        );
        assert_eq!(runs.load(Ordering::SeqCst), 1, "approved call ran");
    }

    #[tokio::test]
    async fn the_registry_is_empty_after_children_end_and_unknown_agents_error() {
        let session = uuid::Uuid::new_v4();
        assert!(steer_agent(session, "missing", "x".into())
            .unwrap_err()
            .contains("no live sub-agent"));
        assert!(kill_agent(session, "missing").is_err());

        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let mock = Arc::new(MockProvider::new().then_text("done"));
        let mut engine = pause_engine(mock, &started, &release);
        let events = capture(&mut engine);
        let owner = ChildOwner::for_session(Some(session));
        delegate_owned(owner, &engine, "task", None, None, 100_000, None)
            .await
            .unwrap();
        let id = started_id(&events).expect("the child announced itself");
        assert!(
            live_agents(session).is_empty(),
            "no live child after the run"
        );
        assert!(steer_agent(session, &id, "late".into()).is_err());
        assert!(kill_agent(session, &id).is_err());
    }

    #[tokio::test]
    async fn a_background_child_reports_its_settlement_once() {
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let mock = Arc::new(MockProvider::new().then_text("the brief"));
        let mut engine = pause_engine(mock, &started, &release);
        let events = capture(&mut engine);
        let session = uuid::Uuid::new_v4();
        let owner = ChildOwner::for_session(Some(session)).detached();
        let report = delegate_owned(owner, &engine, "task", None, None, 100_000, None)
            .await
            .unwrap();
        assert_eq!(report.brief, "the brief");
        let finished: Vec<(bool, String)> = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                ToolActivity::BackgroundFinished { ok, brief, .. } => Some((*ok, brief.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(finished, vec![(true, "the brief".to_string())]);
        assert!(live_agents(session).is_empty());
    }
}
