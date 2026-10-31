//! Agent loop — LEDGER item C1.
//!
//! `Engine` (struct frozen in lib.rs) owns the loop: assemble context →
//! select model → `provider.turn` → dispatch tool calls → repeat until the
//! model stops calling tools, `max_turns` is reached, or the budget governor
//! trips. Every event is appended to the `SessionStore` as it happens, so a
//! crashed run still leaves a usable transcript.

use crate::budget::BudgetGovernor;
use crate::compact;
use crate::context;
use crate::hooks;
use crate::{
    estimate_tokens, judge, ActionKind, ApprovalMode, Engine, Error, JudgeMode, Message,
    ModelChoice, ModelProvider, Result, Role, RunSummary, SelectRequest, SessionEvent, SessionId,
    StopReason, StreamDelta, ToolActivity, ToolCall, ToolCtx, ToolOutcome, ToolSpec, TurnRequest,
    Usage,
};
use std::sync::Arc;

/// Output-token ask sent with every provider turn (`EngineConfig` has no
/// output cap; this is a sane non-streaming default).
const MAX_OUTPUT_TOKENS: u32 = 8192;

/// The mutating tool class gated by [`crate::ApprovalPolicy`] (A1): these
/// change system state (run commands, write files). Everything else
/// (`read`, `glob`, `grep`, `remember`, …) is read-only and never gated.
const MUTATING_TOOLS: [&str; 4] = ["bash", "write", "edit", "verify"];

/// Output-token ask for the compaction (summarizer) call — small by design
/// (PLAN.md §4.10): a compact state is a few hundred tokens at most, and the
/// summary is byte-capped again before it is persisted.
const COMPACTION_MAX_OUTPUT_TOKENS: u32 = 512;

/// Characters of the task used as the session title.
const TITLE_MAX_CHARS: usize = 60;

impl Engine {
    /// Run `task` to completion and return a summary.
    ///
    /// Per turn: assemble the system prompt (config override or the budgeted
    /// default, plus AGENTS.md/CLAUDE.md memory), pre-flight the context
    /// estimate against the per-turn input cap ([`crate::Error::Budget`] on
    /// violation), route via the selector (`ActionKind::Chat` on the first
    /// turn, then derived from the last tool call), call the provider, append
    /// `ModelSelected`/`Usage`/`Message`/`ToolResult` events to the store, and
    /// execute every tool call through the registry.
    ///
    /// Stops cleanly when the model stops requesting tools, when `max_turns`
    /// is reached, or when cumulative reported usage passes
    /// `max_total_tokens`. Tool failures (unknown tool, tool crash) become
    /// error `ToolOutcome`s the model can react to instead of aborting the run.
    pub async fn run_task(&self, task: &str) -> Result<RunSummary> {
        if task.trim().is_empty() {
            return Err(Error::Config(
                "empty task".into(),
                "pass a non-empty task string".into(),
            ));
        }

        let meta = self.store.create_session(&session_title(task)).await?;
        self.store
            .append(
                meta.id,
                &SessionEvent::Task {
                    text: task.to_string(),
                },
            )
            .await?;
        let transcript = vec![Message::user(task)];
        self.run_loop(meta.id, task, transcript).await
    }

    /// Continue an existing session in the SAME session id (LEDGER L1
    /// `--resume`): the transcript is reconstructed from the store's event
    /// log, `task` is appended as a new user message (and `Task` event), and
    /// the loop runs on. Events keep accumulating in one JSONL log, so the
    /// session stays resumable again afterwards.
    ///
    /// Transcript reconstruction: `Task` events become user messages,
    /// `Message` events are replayed as-is, `ToolResult` events become
    /// `Role::Tool` messages with their `call_id`; `ModelSelected`/`Usage`
    /// events carry no transcript content. Compaction-aware: everything
    /// before the LAST `Compaction` event collapses into that event's
    /// compact-state message (plus the verbatim tail that was never
    /// summarized — the same boundary rule the compaction step used), so a
    /// resumed session continues from the compacted context, not the full
    /// history (see [`reconstruct_transcript`]).
    pub async fn run_task_resuming(&self, session_id: SessionId, task: &str) -> Result<RunSummary> {
        if task.trim().is_empty() {
            return Err(Error::Config(
                "empty task".into(),
                "pass a non-empty task string".into(),
            ));
        }

        let events = self.store.load(session_id).await?;
        if events.is_empty() {
            return Err(Error::Session(format!(
                "session {session_id} has no events — nothing to resume; \
                 check the id against `modron --resume <id>` of a completed run"
            )));
        }
        // Rewind support (C2): when `resume_from_event` is set, events after
        // that index stay in the append-only log but never re-enter context.
        let reconstruct_from = match self.config.resume_from_event {
            Some(cut) => &events[..(cut as usize).min(events.len())],
            None => &events[..],
        };
        let mut transcript =
            reconstruct_transcript(reconstruct_from, self.config.compaction.tail_messages);
        self.store
            .append(
                session_id,
                &SessionEvent::Task {
                    text: task.to_string(),
                },
            )
            .await?;
        transcript.push(Message::user(task));
        self.run_loop(session_id, task, transcript).await
    }

    /// Shared turn loop for fresh and resumed runs. `task` is the current
    /// user instruction (used for routing heuristics); `transcript` starts
    /// with that instruction as its last message.
    async fn run_loop(
        &self,
        session_id: SessionId,
        task: &str,
        mut transcript: Vec<Message>,
    ) -> Result<RunSummary> {
        let tool_specs = self.tools.specs();
        let system = self.assemble_system(&tool_specs);
        let mut governor =
            BudgetGovernor::new(self.config.max_total_tokens, self.config.max_input_tokens);

        let mut turns: u32 = 0;
        let mut tool_calls_executed: u32 = 0;
        let mut total = Usage::default();
        let mut models_used: Vec<String> = Vec::new();
        let mut answer = String::new();
        let mut turns_since_compaction: u32 = 0;
        let mut spend_usd = 0.0_f64;

        loop {
            if turns >= self.config.max_turns {
                tracing::warn!(
                    turns,
                    max = self.config.max_turns,
                    "stopping: max_turns reached"
                );
                break;
            }

            // Autocompaction (PLAN.md §4.10): before any turn after the
            // first, under context pressure and past the min-interval,
            // summarize the older transcript. Runs before the pre-flight
            // budget check so a compaction can rescue an over-cap context.
            let mut context_tokens = estimate_context_tokens(&system, &tool_specs, &transcript);
            if self.should_compact(turns, turns_since_compaction, context_tokens) {
                if let Some(outcome) = self.run_compaction(session_id, task).await? {
                    governor.feed_turn(outcome.usage);
                    spend_usd += self.turn_cost_usd(&outcome.model, &outcome.usage);
                    total.input_tokens += outcome.usage.input_tokens;
                    total.output_tokens += outcome.usage.output_tokens;
                    total.cached_input_tokens += outcome.usage.cached_input_tokens;
                    if !models_used.contains(&outcome.model) {
                        models_used.push(outcome.model);
                    }
                    transcript = outcome.transcript;
                    turns_since_compaction = 0;
                    context_tokens = estimate_context_tokens(&system, &tool_specs, &transcript);
                }
            }

            // Pre-flight budget check on the assembled context.
            governor.check_input(context_tokens)?;

            // Route: Chat on the first turn, then derive from the last tool call.
            let action = if turns == 0 {
                ActionKind::Chat
            } else {
                derive_action(&transcript)
            };
            let mut choice = self.selector.select(&SelectRequest {
                task: task.to_string(),
                action,
                context_tokens,
                budget_remaining_usd: self
                    .config
                    .max_cost_usd
                    .map(|cap| (cap - spend_usd).max(0.0)),
            });

            // Async decision model (Phase-3b M1): consulted per JudgeMode.
            // A verdict re-queries the sync selector with the mapped action —
            // tier config, overrides, and budget rules stay in one place.
            if let Some((router, mode)) = &self.config.async_router {
                let consult = match mode {
                    JudgeMode::Always => true,
                    JudgeMode::Ambiguous => !choice.confident,
                };
                if consult {
                    if let Some(verdict) = router.route(task, action).await {
                        let synthetic = SelectRequest {
                            task: task.to_string(),
                            action: judge::verdict_action(verdict),
                            context_tokens,
                            budget_remaining_usd: self
                                .config
                                .max_cost_usd
                                .map(|cap| (cap - spend_usd).max(0.0)),
                        };
                        let mut re = self.selector.select(&synthetic);
                        re.reason = format!("async-router: {verdict:?} → {}", re.reason);
                        choice = re;
                    }
                }
            }
            let (provider, model) = self.resolve_provider(&choice)?;

            self.store
                .append(
                    session_id,
                    &SessionEvent::ModelSelected {
                        model: model.clone(),
                        reason: choice.reason.clone(),
                        action: Some(format!("{action:?}")),
                        context_tokens: Some(context_tokens),
                    },
                )
                .await?;

            let request = TurnRequest {
                model: model.clone(),
                system: system.clone(),
                messages: transcript.clone(),
                tools: tool_specs.clone(),
                max_tokens: MAX_OUTPUT_TOKENS,
            };
            tracing::info!(
                turn = turns + 1,
                provider = provider.id(),
                model = %model,
                "turn start"
            );
            // Streaming dispatch (Phase-1c I1): when a delta sink is
            // configured, drive the provider through `turn_streaming` and
            // forward deltas as they arrive (providers without an SSE
            // implementation yield one Text delta via the trait default).
            let response = match self.config.on_delta.clone() {
                Some(sink) => {
                    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<StreamDelta>();
                    let forwarder = tokio::spawn(async move {
                        while let Some(delta) = rx.recv().await {
                            sink(delta);
                        }
                    });
                    let result = provider.turn_streaming(request, tx).await;
                    let _ = forwarder.await;
                    result?
                }
                None => provider.turn(request).await?,
            };
            turns += 1;

            // Usage: provider-reported ground truth, never estimates.
            governor.feed_turn(response.usage);
            total.input_tokens += response.usage.input_tokens;
            total.output_tokens += response.usage.output_tokens;
            total.cached_input_tokens += response.usage.cached_input_tokens;
            self.store
                .append(
                    session_id,
                    &SessionEvent::Usage {
                        model: model.clone(),
                        usage: response.usage,
                    },
                )
                .await?;
            if !models_used.contains(&model) {
                models_used.push(model.clone());
            }
            spend_usd += self.turn_cost_usd(&model, &response.usage);

            // USD spend cap (A3): computed from provider-reported usage and
            // the injected price lookup — an honest estimate, checked after
            // the turn so the turn's real usage is what trips it.
            if let Some(cap) = self.config.max_cost_usd {
                if spend_usd > cap {
                    tracing::error!(
                        spend = format!("${spend_usd:.4}"),
                        cap = format!("${cap:.4}"),
                        "stopping: USD spend cap exceeded"
                    );
                    answer = format!(
                        "{answer}\n\n[stopped: USD spend cap exceeded — spent ~${spend_usd:.4} of ${cap:.4} cap]"
                    )
                    .trim()
                    .to_string();
                    break;
                }
            }

            // Record the assistant message (text and/or tool calls).
            let assistant = Message {
                role: Role::Assistant,
                content: response.text.clone(),
                tool_calls: response.tool_calls.clone(),
                tool_call_id: None,
            };
            self.store
                .append(
                    session_id,
                    &SessionEvent::Message {
                        parent_id: None,
                        message: assistant.clone(),
                    },
                )
                .await?;
            transcript.push(assistant);
            answer = response.text;

            if response.stop_reason != StopReason::ToolUse || response.tool_calls.is_empty() {
                break;
            }

            // Dispatch tool calls; each result enters the transcript. Hooks
            // (H1) wrap execution: pre_tool may veto before the tool runs,
            // post_tool observes after. Hook-spawn errors fail closed.
            // Checkpoints (C1) capture the prior content of files that
            // write/edit are about to mutate — only in ApproveAll mode, since
            // any other mode blocks these tools before they can mutate.
            let ctx = ToolCtx {
                sandbox: self.config.sandbox,
                working_dir: self.config.working_dir.clone(),
                engine: Some(Arc::new(self.clone())),
            };
            // Phase 4: when every call in the turn is read-only, they run
            // concurrently (order-preserving — the transcript is identical to
            // the sequential case, only faster). Mutating calls stay
            // sequential: checkpoints and approval gating are order-sensitive.
            let all_read_only = response
                .tool_calls
                .iter()
                .all(|c| !MUTATING_TOOLS.contains(&c.name.as_str()));
            let outcomes: Vec<ToolOutcome> = if all_read_only && response.tool_calls.len() > 1 {
                let futures = response
                    .tool_calls
                    .iter()
                    .map(|call| self.execute_tool_with_hooks(session_id, call, &ctx));
                futures::future::join_all(futures)
                    .await
                    .into_iter()
                    .map(|r| r.unwrap_or_else(|err| ToolOutcome::err(err.to_string())))
                    .collect()
            } else {
                let mut outcomes = Vec::with_capacity(response.tool_calls.len());
                for call in &response.tool_calls {
                    if self.config.approvals.mode == ApprovalMode::ApproveAll {
                        if let Some(checkpoint) = self.checkpoint_for(session_id, call).await? {
                            self.store.append(session_id, &checkpoint).await?;
                        }
                    }
                    let outcome = self
                        .execute_tool_with_hooks(session_id, call, &ctx)
                        .await
                        .unwrap_or_else(|err| ToolOutcome::err(err.to_string()));
                    outcomes.push(outcome);
                }
                outcomes
            };
            for (call, outcome) in response.tool_calls.iter().zip(&outcomes) {
                tracing::info!(
                    tool = %call.name,
                    error = outcome.is_error,
                    result_bytes = outcome.content.len(),
                    "tool done"
                );
                self.store
                    .append(
                        session_id,
                        &SessionEvent::ToolResult {
                            call_id: call.id.clone(),
                            outcome: outcome.clone(),
                        },
                    )
                    .await?;
                transcript.push(Message::tool_result(call.id.clone(), outcome));
                tool_calls_executed += 1;
            }

            // A main turn completed: it counts toward the compaction
            // min-interval (the compaction call itself is not a turn).
            turns_since_compaction += 1;

            if governor.exceeded() {
                tracing::warn!(
                    cumulative = governor.cumulative(),
                    cap = self.config.max_total_tokens,
                    "stopping: cumulative token budget exceeded"
                );
                break;
            }
        }

        Ok(RunSummary {
            session_id,
            answer,
            turns,
            tool_calls: tool_calls_executed,
            usage: total,
            models_used,
        })
    }

    /// Cost of one turn in USD from provider-reported usage and the injected
    /// [`PriceLookup`](crate::PriceLookup). No lookup or unknown model → $0
    /// (warned once per run from the caller's perspective is the CLI's job;
    /// here it is silent to avoid log spam).
    fn turn_cost_usd(&self, model: &str, usage: &Usage) -> f64 {
        let Some(lookup) = self.config.price_lookup.as_ref() else {
            return 0.0;
        };
        match lookup(model) {
            Some((in_per_m, out_per_m)) => {
                usage.input_tokens as f64 / 1_000_000.0 * in_per_m
                    + usage.output_tokens as f64 / 1_000_000.0 * out_per_m
            }
            None => 0.0,
        }
    }

    /// System prompt for the whole run: `config.system` if the caller set one,
    /// otherwise the budgeted default from [`context::build_system_prompt`].
    /// AGENTS.md/CLAUDE.md memory from the working directory is appended in
    /// both cases, and — when `config.memory_root` is set (Phase-1a lead
    /// amendment) — the workspace's user-memory section on top of it
    /// ([`context::with_user_memory`]). Stable prefix: assembled once, reused
    /// across turns; an empty/missing memory contributes nothing, so the
    /// prompt is byte-identical to the memory-less assembly then.
    fn assemble_system(&self, tools: &[ToolSpec]) -> String {
        let memory = context::load_memory(&self.config.working_dir);
        let base = if self.config.system.is_empty() {
            context::build_system_prompt(tools, "")
        } else {
            self.config.system.clone()
        };
        let memory_block = context::memory_section(&memory);
        let system = if memory_block.is_empty() {
            base
        } else {
            format!("{base}{memory_block}")
        };
        // Skills index (Phase 4): names + descriptions only; the model loads
        // a body via the `skill` tool when it wants the workflow.
        let system = match context::load_skills_index(&self.config.working_dir) {
            index if index.is_empty() => system,
            index => format!(
                "{system}\n\n## Skills\n{index}\nCall the `skill` tool to load full instructions before following one."
            ),
        };
        match &self.config.memory_root {
            Some(root) => context::with_user_memory(
                &system,
                &context::load_user_memory(root, &self.config.working_dir),
            ),
            None => system,
        }
    }

    /// Resolve (provider, model) for a selector choice, falling back to the
    /// configured defaults when the selector names an unregistered provider.
    fn resolve_provider(&self, choice: &ModelChoice) -> Result<(Arc<dyn ModelProvider>, String)> {
        if let Some(provider) = self.providers.get(&choice.provider) {
            return Ok((provider.clone(), choice.model.clone()));
        }
        if let Some(provider) = self.providers.get(&self.config.default_provider) {
            tracing::debug!(
                requested = %choice.provider,
                fallback = %self.config.default_provider,
                "selector chose an unregistered provider; using default"
            );
            return Ok((provider.clone(), self.config.default_model.clone()));
        }
        Err(Error::Config(
            format!(
                "no provider registered under `{}` (or default `{}`)",
                choice.provider, self.config.default_provider
            ),
            "register providers in Engine.providers keyed by provider id \
             (\"anthropic\", \"openai-compat\", \"mock\")"
                .into(),
        ))
    }

    /// Execute one tool call through the registry, converting hard failures
    /// (unknown tool, tool crash) into error outcomes so the model can react
    /// instead of the run dying. Mutating tools ([`MUTATING_TOOLS`]) pass the
    /// approval gate first (A1): anything short of `ApproveAll` blocks the
    /// execution and hands the model an error outcome it can adapt to.
    /// Build the pre-mutation `Checkpoint` event for a tool call (C1): only
    /// `write`/`edit` are checkpointed in v1 (bash mutations are a documented
    /// limitation — use git). `event_index` is the log position the following
    /// `ToolResult` will occupy, computed from a fresh store load so the
    /// count stays correct across compactions and resumes.
    async fn checkpoint_for(
        &self,
        session_id: SessionId,
        call: &ToolCall,
    ) -> Result<Option<SessionEvent>> {
        const SNAPSHOT_CAP: usize = 256 * 1024;
        if !matches!(call.name.as_str(), "write" | "edit") {
            return Ok(None);
        }
        let Some(path) = call.arguments.get("path").and_then(|v| v.as_str()) else {
            return Ok(None);
        };
        let absolute = if std::path::Path::new(path).is_absolute() {
            std::path::PathBuf::from(path)
        } else {
            self.config.working_dir.join(path)
        };
        let (prior, warned) = match std::fs::read(&absolute) {
            Ok(bytes) if bytes.len() <= SNAPSHOT_CAP => (Some(crate::b64::encode(&bytes)), false),
            Ok(bytes) => {
                tracing::warn!(
                    path = %path,
                    bytes = bytes.len(),
                    "file exceeds 256KB checkpoint cap — rewind cannot restore it"
                );
                (None, true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (None, false),
            Err(e) => {
                tracing::warn!(path = %path, error = %e, "cannot read file for checkpoint");
                (None, true)
            }
        };
        let _ = warned;
        let event_index = (self.store.load(session_id).await?.len() + 1) as u64;
        Ok(Some(SessionEvent::Checkpoint {
            path: path.to_string(),
            prior_content_b64: prior,
            event_index,
        }))
    }

    /// Dispatch one tool call through the hook pipeline (H1): a `pre_tool`
    /// hook may veto the call (its stderr tail becomes the model-visible
    /// error, fail-closed on hook infrastructure errors), then the tool
    /// executes (which includes the approval gate), then `post_tool`
    /// observes — its failures are logged, never blocking. Note: hooks run
    /// *before* the approval gate inside the tool execution; a vetoed call
    /// never reaches it.
    async fn execute_tool_with_hooks(
        &self,
        session_id: SessionId,
        call: &ToolCall,
        ctx: &ToolCtx,
    ) -> Result<ToolOutcome> {
        let hook_envs = |exit: Option<u8>| {
            let mut envs: Vec<(String, String)> = vec![
                ("MODRON_TOOL_NAME".to_string(), call.name.clone()),
                (
                    "MODRON_TOOL_ARGS".to_string(),
                    serde_json::to_string(&call.arguments).unwrap_or_else(|_| "{}".to_string()),
                ),
                ("MODRON_SESSION_ID".to_string(), session_id.to_string()),
            ];
            if let Some(code) = exit {
                envs.push(("MODRON_TOOL_EXIT_CODE".to_string(), code.to_string()));
            }
            envs
        };

        if let Some(pre) = self.config.hooks.pre_tool.clone() {
            let outcome = hooks::run_hook(&pre, &hook_envs(None), hooks::HOOK_TIMEOUT).await?;
            if outcome.blocked {
                return Ok(ToolOutcome::err(format!(
                    "blocked by pre_tool hook: {}",
                    outcome.message
                )));
            }
        }

        if let Some(sink) = self.config.on_tool_event.as_ref() {
            sink(ToolActivity::Start {
                name: call.name.clone(),
            });
        }
        let executed = self.execute_tool(call, ctx).await;
        if let Some(sink) = self.config.on_tool_event.as_ref() {
            sink(ToolActivity::Done {
                name: call.name.clone(),
                is_error: executed.is_error,
                bytes: executed.content.len(),
            });
        }

        if let Some(post) = self.config.hooks.post_tool.clone() {
            let code = u8::from(executed.is_error);
            if let Err(err) =
                hooks::run_hook(&post, &hook_envs(Some(code)), hooks::HOOK_TIMEOUT).await
            {
                tracing::warn!(tool = %call.name, error = %err, "post_tool hook failed (non-blocking)");
            }
        }
        Ok(executed)
    }

    async fn execute_tool(&self, call: &ToolCall, ctx: &ToolCtx) -> ToolOutcome {
        let Some(tool) = self.tools.get(&call.name).cloned() else {
            let available = self
                .tools
                .specs()
                .iter()
                .map(|spec| spec.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return ToolOutcome::err(format!(
                "unknown tool `{}` — available tools: {available}",
                call.name
            ));
        };

        // Approval gate (A1): mutating tools never execute unless approved.
        // Deny list: any tool, any mode, always wins.
        if self.config.approvals.deny.iter().any(|d| d == &call.name) {
            return ToolOutcome::err(format!(
                "tool `{}` denied by configuration (--deny)",
                call.name
            ));
        }
        // Allow list bypasses Ask mode (deny already checked).
        let explicitly_allowed = self.config.approvals.allow.iter().any(|a| a == &call.name);
        if MUTATING_TOOLS.contains(&call.name.as_str())
            && !explicitly_allowed
            && self.config.approvals.mode != ApprovalMode::ApproveAll
        {
            // Ask mode with an interactive approver (Phase-1c I2): offer the
            // call to the callback; a denial falls through to the block.
            let approved = self.config.approvals.mode == ApprovalMode::Ask
                && self
                    .config
                    .approver
                    .as_ref()
                    .is_some_and(|approver| approver(call));
            if !approved {
                let mode = self.config.approvals.mode;
                tracing::warn!(
                    tool = %call.name,
                    mode = ?mode,
                    "tool blocked by approval mode — run with --approve-all to allow mutating tools"
                );
                let hint = match mode {
                    ApprovalMode::Deny => "blocked by approval mode: deny".to_string(),
                    _ => "blocked by approval mode: ask (non-interactive run — rerun with \
                          --approve-all to allow mutating tools)"
                        .to_string(),
                };
                return ToolOutcome::err(hint);
            }
        }

        match tool.execute(call.arguments.clone(), ctx).await {
            Ok(outcome) => outcome,
            Err(err) => ToolOutcome::err(format!("tool `{}` failed to run: {err}", call.name)),
        }
    }

    /// Whether the autocompaction trigger fires before this turn (X1): never
    /// on the first turn, only when the policy is enabled, under context
    /// pressure (estimate > `threshold_frac × max_input_tokens`), and past
    /// the min-interval since the last compaction.
    fn should_compact(&self, turns: u32, turns_since_compaction: u32, context_tokens: u64) -> bool {
        let policy = &self.config.compaction;
        policy.enabled
            && turns > 0
            && turns_since_compaction >= policy.min_interval_turns
            && (context_tokens as f64)
                > policy.threshold_frac * (self.config.max_input_tokens as f64)
    }

    /// One compaction pass (X1, PLAN.md §4.10): load the session's events,
    /// render everything older than the policy tail
    /// ([`compact::older_transcript_text`]), summarize it with a routed
    /// cheap-model call (`ActionKind::Summarize`, small output cap, no
    /// tools), cap the summary ([`compact::cap_summary`]), append the
    /// `Compaction` event plus its `Usage` event, and return the rebuilt
    /// transcript: `[compact state] + recent tail verbatim`.
    ///
    /// `Ok(None)` means there was nothing older than the tail to summarize —
    /// no provider call, no events, no interval reset. The caller feeds the
    /// returned usage into the budget governor and run totals.
    async fn run_compaction(
        &self,
        session_id: SessionId,
        task: &str,
    ) -> Result<Option<CompactionOutcome>> {
        let events = self.store.load(session_id).await?;
        let (older, tail) =
            compact::split_older_tail(&events, self.config.compaction.tail_messages);
        let summarized_count = older
            .iter()
            .filter(|event| is_transcript_item(event))
            .count();
        if summarized_count == 0 {
            tracing::debug!(session = %session_id, "compaction skipped: nothing older than the tail");
            return Ok(None);
        }
        let summarized_events = u32::try_from(summarized_count).unwrap_or(u32::MAX);
        let older_text =
            compact::older_transcript_text(&events, self.config.compaction.tail_messages);

        // Route the summarization (cheap model for Summarize per PLAN §4.3).
        let choice = self.selector.select(&SelectRequest {
            task: task.to_string(),
            action: ActionKind::Summarize,
            context_tokens: estimate_tokens(&older_text),
            budget_remaining_usd: None,
        });
        let (provider, model) = self.resolve_provider(&choice)?;

        let request = TurnRequest {
            model: model.clone(),
            system: compact::summarizer_prompt().to_string(),
            messages: vec![Message::user(older_text)],
            tools: Vec::new(),
            max_tokens: COMPACTION_MAX_OUTPUT_TOKENS,
        };
        tracing::info!(
            provider = provider.id(),
            model = %model,
            summarized_events,
            "compaction turn: summarizing older transcript"
        );
        let response = provider.turn(request).await?;

        // Cap the summary before it is persisted (char boundary + marker).
        let summary =
            compact::cap_summary(&response.text, self.config.compaction.summary_max_bytes);

        self.store
            .append(
                session_id,
                &SessionEvent::Compaction {
                    summary: summary.clone(),
                    summarized_events,
                },
            )
            .await?;
        self.store
            .append(
                session_id,
                &SessionEvent::Usage {
                    model: model.clone(),
                    usage: response.usage,
                },
            )
            .await?;

        // Rebuild the in-memory transcript: one compact-state user message
        // replaces everything summarized, then the recent tail verbatim.
        let mut transcript = Vec::with_capacity(tail.len() + 1);
        transcript.push(compact_state_message(&summary));
        transcript.extend(events_to_messages(tail));

        // Debug-level: the CLI surfaces the user-facing `compacted: …
        // (est. saved ~… tok)` line itself (X3); this is the verbose detail.
        tracing::debug!(session = %session_id, summarized_events, "compaction summarized the older transcript");

        Ok(Some(CompactionOutcome {
            transcript,
            usage: response.usage,
            model,
        }))
    }
}

/// Result of one compaction pass: the rebuilt in-memory transcript plus the
/// compaction call's provider-reported usage and model for accounting.
struct CompactionOutcome {
    transcript: Vec<Message>,
    usage: Usage,
    model: String,
}

/// The user-role message that replaces the summarized older transcript. The
/// label is byte-stable: resumed sessions and tests match on it.
fn compact_state_message(summary: &str) -> Message {
    Message::user(format!(
        "Conversation compacted; summary of earlier work:\n{summary}"
    ))
}

/// Whether the event carries transcript content (counts toward the tail).
fn is_transcript_item(event: &SessionEvent) -> bool {
    matches!(
        event,
        SessionEvent::Task { .. } | SessionEvent::Message { .. } | SessionEvent::ToolResult { .. }
    )
}

/// Convert session events to transcript messages. A `Compaction` event
/// becomes the compact-state user message it stands for (so a verbatim tail
/// that spans an older compaction keeps its state); accounting events are
/// dropped.
fn events_to_messages<'a>(events: impl IntoIterator<Item = &'a SessionEvent>) -> Vec<Message> {
    let mut messages = Vec::new();
    for event in events {
        match event {
            SessionEvent::Task { text } => messages.push(Message::user(text.clone())),
            SessionEvent::Message { message, .. } => messages.push(message.clone()),
            SessionEvent::ToolResult { call_id, outcome } => {
                messages.push(Message::tool_result(call_id.clone(), outcome));
            }
            SessionEvent::Compaction { summary, .. } => {
                messages.push(compact_state_message(summary));
            }
            SessionEvent::Checkpoint { .. }
            | SessionEvent::ModelSelected { .. }
            | SessionEvent::Usage { .. } => {}
        }
    }
    messages
}

/// Rebuild the linear transcript from a session's event log (see
/// [`Engine::run_task_resuming`]).
///
/// Compaction-aware: everything before the LAST `Compaction` event collapses
/// into that event's compact-state message, plus the verbatim tail that was
/// never summarized — recomputed with the same boundary rule the compaction
/// step used ([`compact::split_older_tail`]), so items the orphan fold moved
/// into the summary are not replayed — followed by every event after the
/// compaction. Sessions without compaction replay exactly as before.
fn reconstruct_transcript(events: &[SessionEvent], tail_messages: usize) -> Vec<Message> {
    let last_compaction = events
        .iter()
        .enumerate()
        .rev()
        .find_map(|(idx, event)| match event {
            SessionEvent::Compaction { summary, .. } => Some((idx, summary)),
            _ => None,
        });
    let Some((idx, summary)) = last_compaction else {
        return events_to_messages(events);
    };

    let mut messages = vec![compact_state_message(summary)];
    let (_, tail) = compact::split_older_tail(&events[..idx], tail_messages);
    messages.extend(events_to_messages(tail));
    messages.extend(events_to_messages(&events[idx + 1..]));
    messages
}

/// First line of the task, trimmed to ~[`TITLE_MAX_CHARS`] characters.
fn session_title(task: &str) -> String {
    let first_line = task.trim().lines().next().unwrap_or("").trim();
    let mut title: String = first_line.chars().take(TITLE_MAX_CHARS).collect();
    if first_line.chars().count() > TITLE_MAX_CHARS {
        title.push('…');
    }
    if title.is_empty() {
        title = "(empty task)".into();
    }
    title
}

/// Derive the routing action from the most recent tool call: mutating or
/// executing tools route to `Edit`, read-only search tools to `Search`,
/// anything else to `Chat`. Deliberately simple — richer heuristics live in
/// the router crate.
fn derive_action(messages: &[Message]) -> ActionKind {
    for message in messages.iter().rev() {
        if let Some(call) = message.tool_calls.last() {
            let name = call.name.as_str();
            return if name.contains("edit") || name.contains("write") || name.contains("bash") {
                ActionKind::Edit
            } else if name.contains("grep")
                || name.contains("glob")
                || name.contains("read")
                || name.contains("search")
                || name.contains("find")
            {
                ActionKind::Search
            } else {
                ActionKind::Chat
            };
        }
    }
    ActionKind::Chat
}

/// Pre-flight estimate of the tokens the next provider call will see:
/// system prompt + tool schemas + full transcript (incl. tool-call JSON).
fn estimate_context_tokens(system: &str, tools: &[ToolSpec], messages: &[Message]) -> u64 {
    let mut tokens = estimate_tokens(system);
    if let Ok(json) = serde_json::to_string(tools) {
        tokens += estimate_tokens(&json);
    }
    for message in messages {
        tokens += estimate_tokens(&message.content);
        for call in &message.tool_calls {
            if let Ok(json) = serde_json::to_string(call) {
                tokens += estimate_tokens(&json);
            }
        }
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn title_is_first_line_truncated() {
        assert_eq!(session_title("fix the bug"), "fix the bug");
        let long = format!("{}\nsecond line", "x".repeat(100));
        let title = session_title(&long);
        assert_eq!(title.chars().count(), TITLE_MAX_CHARS + 1); // 60 chars + ellipsis
        assert!(title.ends_with('…'));
    }

    #[test]
    fn action_derivation_from_tool_names() {
        let search = vec![assistant_with_call("grep")];
        assert_eq!(derive_action(&search), ActionKind::Search);
        let edit = vec![assistant_with_call("edit")];
        assert_eq!(derive_action(&edit), ActionKind::Edit);
        let bash = vec![assistant_with_call("bash")];
        assert_eq!(derive_action(&bash), ActionKind::Edit);
        assert_eq!(derive_action(&[Message::user("hi")]), ActionKind::Chat);
        // No tool call at all in newer messages either.
        assert_eq!(derive_action(&[]), ActionKind::Chat);
    }

    fn assistant_with_call(name: &str) -> Message {
        let mut message = Message::assistant("working");
        message.tool_calls = vec![ToolCall {
            id: "1".into(),
            name: name.into(),
            arguments: json!({}),
        }];
        message
    }

    #[test]
    fn estimate_covers_system_tools_and_transcript() {
        let tools = vec![ToolSpec {
            name: "t".into(),
            description: "d".into(),
            parameters: json!({"type": "object"}),
        }];
        let messages = vec![Message::user("abcd")]; // 1 token
        let est = estimate_context_tokens("abcdefgh", &tools, &messages); // 2 tokens + tools
        assert!(
            est > 3,
            "estimate must include the tool schema cost, got {est}"
        );
    }
}
