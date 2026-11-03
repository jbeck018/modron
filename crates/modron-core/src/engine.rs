//! Agent loop — LEDGER item C1.
//!
//! `Engine` (struct frozen in lib.rs) owns the loop: assemble context →
//! select model → `provider.turn` → dispatch tool calls → repeat until the
//! model stops calling tools, `max_turns` is reached, or the budget governor
//! trips. Every event is appended to the `SessionStore` as it happens, so a
//! crashed run still leaves a usable transcript.

use crate::bitmap_compact;
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

/// Finish-gate bound: verification feedback rounds before the run may end
/// without passing verification (StaminaBench: gains plateau after 3-5
/// retries; 2 keeps the worst case tight).
const MAX_VERIFY_ROUNDS: u32 = 2;

/// Idempotent, side-effect-free tools eligible for speculative execution
/// (started mid-stream when the provider completes a tool-call item early,
/// reused by the dispatch once the turn finishes — oh-my-pi pattern).
/// bash/write/edit stay out: side effects must never run on a call the
/// model might still revise.
const SPECULATIVE_TOOLS: [&str; 3] = ["read", "grep", "glob"];

/// Identity key for a speculative result: (tool, exact wire args).
fn speculative_key(call: &ToolCall) -> String {
    format!("{}:{}", call.name, call.arguments)
}

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
        self.run_task_with_attachments(task, Vec::new()).await
    }

    /// Run a task with multimodal attachments (Phase 7): images and
    /// documents ride on the first user message.
    pub async fn run_task_with_attachments(
        &self,
        task: &str,
        attachments: Vec<crate::Attachment>,
    ) -> Result<RunSummary> {
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
        let transcript = vec![Message::user_with_attachments(task, attachments)];
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
        self.run_task_resuming_with_attachments(session_id, task, Vec::new())
            .await
    }

    /// Continue a session with multimodal attachments (Phase 14 image
    /// paste): the attachments ride on the new user message.
    pub async fn run_task_resuming_with_attachments(
        &self,
        session_id: SessionId,
        task: &str,
        attachments: Vec<crate::Attachment>,
    ) -> Result<RunSummary> {
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
        transcript.push(Message::user_with_attachments(task, attachments));
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
        let tool_specs = self.tools.specs_prompt();
        let system = self.assemble_system(&tool_specs);
        // Cache-discipline audit key: one hash over the exact wire prefix
        // (system + tool specs) — a change across turns means prefix
        // instability (cache busts), visible in TurnTrace records.
        let prompt_hash = prompt_wire_hash(&system, &tool_specs);
        let mut stop_label: &'static str = "end_turn";
        let mut governor =
            BudgetGovernor::new(self.config.max_total_tokens, self.config.max_input_tokens);

        let mut turns: u32 = 0;
        let mut tool_calls_executed: u32 = 0;
        let mut total = Usage::default();
        let mut models_used: Vec<String> = Vec::new();
        let mut answer = String::new();
        let mut turns_since_compaction: u32 = 0;
        // Phase 13: memory ids this run surfaced via memory_search — folded
        // back into the usefulness ledger when the run completes.
        let mut memory_hits: Vec<String> = Vec::new();
        let mut spend_usd = 0.0_f64;
        // Finish-gate state: whether a mutating tool was dispatched this
        // run, and how many verification rounds have been spent.
        let mut mutated_this_run = false;
        let mut verify_rounds: u32 = 0;
        // Whether the most recent dispatch turn ENDED with a passing
        // `verify` call — the model's own evidence satisfies the gate.
        let mut last_dispatch_ended_with_passing_verify = false;

        loop {
            if turns >= self.config.max_turns {
                tracing::warn!(
                    turns,
                    max = self.config.max_turns,
                    "stopping: max_turns reached"
                );
                stop_label = "max_turns";
                break;
            }

            // Route FIRST (Phase 5i): the selected model's context window
            // (from ModelChoice) is the compaction trigger — a 128k-window
            // free model must compact at 0.8×128k even when the global cap
            // is 180k.
            let mut context_tokens = estimate_context_tokens(&system, &tool_specs, &transcript);
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
            let effective_cap = choice
                .context_window
                .unwrap_or(self.config.max_input_tokens)
                .min(self.config.max_input_tokens);

            // Autocompaction (PLAN.md §4.10): before any turn after the
            // first, under context pressure and past the min-interval,
            // summarize the older transcript. Runs before the pre-flight
            // budget check so a compaction can rescue an over-cap context.
            // The current selection is the model taking the NEXT turn —
            // passed in so the bitmap attachment can be gated on its
            // vision capability.
            if self.should_compact_with_cap(
                turns,
                turns_since_compaction,
                context_tokens,
                effective_cap,
            ) {
                if let Some(outcome) = self.run_compaction(session_id, task, &choice.model).await? {
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
                    // The context changed materially: re-route once.
                    choice = self.selector.select(&SelectRequest {
                        task: task.to_string(),
                        action,
                        context_tokens,
                        budget_remaining_usd: self
                            .config
                            .max_cost_usd
                            .map(|cap| (cap - spend_usd).max(0.0)),
                    });
                }
            }

            // Pre-flight budget check on the assembled context, against the
            // effective cap (the model's window when known).
            governor.check_input_with_cap(context_tokens, effective_cap)?;

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
            let turn_started = std::time::Instant::now();
            // Speculative results computed during THIS turn's stream.
            let mut speculative: std::collections::HashMap<String, ToolOutcome> =
                std::collections::HashMap::new();
            let response = match self.config.on_delta.clone() {
                Some(sink) => {
                    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<StreamDelta>();
                    // Speculative driver: forwards deltas to the sink,
                    // and when the provider completes a tool-call item
                    // mid-stream, immediately executes idempotent reads
                    // so the dispatch below can reuse the result instead
                    // of re-running the tool. Gated OFF when any observer
                    // or gate would be bypassed: pre_tool/post_tool
                    // hooks, the on_tool_event sink, and the --deny list
                    // all must keep seeing every execution.
                    let speculation_safe = self.config.hooks.pre_tool.is_none()
                        && self.config.hooks.post_tool.is_none()
                        && self.config.on_tool_event.is_none()
                        && !self
                            .config
                            .approvals
                            .deny
                            .iter()
                            .any(|d| SPECULATIVE_TOOLS.contains(&d.as_str()));
                    if speculation_safe {
                        let spec_results: std::sync::Arc<
                            std::sync::Mutex<std::collections::HashMap<String, ToolOutcome>>,
                        > = std::sync::Arc::default();
                        let engine_for_spec = Arc::new(self.clone());
                        let driver_map = Arc::clone(&spec_results);
                        // The driver owns the in-flight handles so every
                        // speculation is joined before the drain below.
                        let driver = tokio::spawn(async move {
                            let mut in_flight: Vec<tokio::task::JoinHandle<()>> = Vec::new();
                            while let Some(delta) = rx.recv().await {
                                match delta {
                                    StreamDelta::ToolCallReady(call) => {
                                        if !SPECULATIVE_TOOLS.contains(&call.name.as_str()) {
                                            continue;
                                        }
                                        let key = speculative_key(&call);
                                        // Placeholder first: duplicate
                                        // items for the same call must
                                        // not spawn duplicate executions.
                                        {
                                            let mut map = driver_map.lock().unwrap();
                                            if map.contains_key(&key) {
                                                continue;
                                            }
                                            map.insert(
                                                key.clone(),
                                                ToolOutcome::err(String::new()),
                                            );
                                        }
                                        let engine = Arc::clone(&engine_for_spec);
                                        let map = Arc::clone(&driver_map);
                                        in_flight.push(tokio::spawn(async move {
                                            let name = call.name.clone();
                                            let ctx = ToolCtx {
                                                sandbox: engine.config.sandbox,
                                                working_dir: engine.config.working_dir.clone(),
                                                engine: Some(Arc::clone(&engine)),
                                            };
                                            let outcome = match engine.tools.get(&name) {
                                                Some(tool) => tool
                                                    .execute(call.arguments.clone(), &ctx)
                                                    .await
                                                    .unwrap_or_else(|e| {
                                                        ToolOutcome::err(e.to_string())
                                                    }),
                                                None => ToolOutcome::err("unknown tool"),
                                            };
                                            map.lock()
                                                .unwrap()
                                                .insert(speculative_key(&call), outcome);
                                        }));
                                    }
                                    other => sink(other),
                                }
                            }
                            in_flight
                        });
                        let result = provider.turn_streaming(request, tx).await;
                        let in_flight = driver.await.unwrap_or_default();
                        for handle in in_flight {
                            let _ = handle.await;
                        }
                        let drained: Vec<_> = spec_results.lock().unwrap().drain().collect();
                        speculative.extend(drained);
                        result?
                    } else {
                        // Plain forwarder: speculation disabled (hooks,
                        // event sink, or deny list present).
                        let forwarder = tokio::spawn(async move {
                            while let Some(delta) = rx.recv().await {
                                sink(delta);
                            }
                        });
                        let result = provider.turn_streaming(request, tx).await;
                        let _ = forwarder.await;
                        result?
                    }
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
            let turn_cost = self.turn_cost_usd(&model, &response.usage);
            spend_usd += turn_cost;
            self.store
                .append(
                    session_id,
                    &SessionEvent::TurnTrace {
                        turn: turns,
                        harness_version: env!("CARGO_PKG_VERSION").to_string(),
                        provider: provider.id().to_string(),
                        model: model.clone(),
                        prompt_hash: Some(prompt_hash.clone()),
                        latency_ms: Some(turn_started.elapsed().as_millis() as u64),
                        cost_usd: Some(turn_cost),
                        usage: Some(response.usage),
                        verifier: None,
                        failure_label: None,
                    },
                )
                .await?;

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
                    stop_label = "spend_cap";
                    break;
                }
            }

            // Record the assistant message (text and/or tool calls).
            let assistant = Message {
                role: Role::Assistant,
                content: response.text.clone(),
                tool_calls: response.tool_calls.clone(),
                tool_call_id: None,
                attachments: Vec::new(),
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
                // Finish gate (harness-enforced verification): a run that
                // dispatched mutating tools must show verify evidence
                // before it may end — outcome-only scoring hides unverified
                // work (AgentLens: 94.3% of lucky passes lack it). Arms
                // only when a `verify` tool is registered (i.e. [verify]
                // configured); failures feed the output back verbatim and
                // continue — StaminaBench's retry-with-test-feedback loop,
                // bounded to MAX_VERIFY_ROUNDS per run.
                if !self
                    .run_finish_gate(
                        session_id,
                        &mut transcript,
                        &mut mutated_this_run,
                        &mut verify_rounds,
                        &mut answer,
                        last_dispatch_ended_with_passing_verify,
                    )
                    .await?
                {
                    break;
                }
                continue;
            }

            // A requested mutating call arms the finish gate (conservative:
            // a vetoed or failed call still counts — the model ATTEMPTED a
            // mutation, so unverified finishes stay blocked).
            if response
                .tool_calls
                .iter()
                .any(|c| MUTATING_TOOLS.contains(&c.name.as_str()))
            {
                mutated_this_run = true;
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
                    .map(|call| self.dispatch_tool_call(session_id, call, &ctx, &speculative));
                futures::future::join_all(futures).await
            } else {
                let mut outcomes = Vec::with_capacity(response.tool_calls.len());
                for call in &response.tool_calls {
                    if self.config.approvals.mode == ApprovalMode::ApproveAll {
                        if let Some(checkpoint) = self.checkpoint_for(session_id, call).await? {
                            self.store.append(session_id, &checkpoint).await?;
                        }
                    }
                    let outcome = self
                        .dispatch_tool_call(session_id, call, &ctx, &speculative)
                        .await;
                    outcomes.push(outcome);
                }
                outcomes
            };
            last_dispatch_ended_with_passing_verify = response
                .tool_calls
                .last()
                .map(|c| c.name == "verify")
                .unwrap_or(false)
                && outcomes.last().map(|o| !o.is_error).unwrap_or(false);

            for (call, outcome) in response.tool_calls.iter().zip(&outcomes) {
                tracing::info!(
                    tool = %call.name,
                    error = outcome.is_error,
                    result_bytes = outcome.content.len(),
                    "tool done"
                );
                if let Some(ids) = outcome
                    .meta
                    .as_ref()
                    .and_then(|m| m.get("memory_hits"))
                    .and_then(|v| v.as_array())
                {
                    memory_hits.extend(ids.iter().filter_map(|i| i.as_str().map(String::from)));
                }
                // Delegated spend (Phase 5c A2): usage spent behind a tool's
                // interface joins the governor and the run totals — honest
                // accounting to the token.
                if let Some(usage) = outcome.usage {
                    governor.feed_turn(usage);
                    spend_usd += self.turn_cost_usd(&model, &usage);
                    total.input_tokens += usage.input_tokens;
                    total.output_tokens += usage.output_tokens;
                    total.cached_input_tokens += usage.cached_input_tokens;
                    self.store
                        .append(
                            session_id,
                            &SessionEvent::Usage {
                                model: format!("subagent ({})", call.name),
                                usage,
                            },
                        )
                        .await?;
                }
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
                stop_label = "budget_exhausted";
                break;
            }
        }

        // Phase 13 self-improving memory: a completed run is a success
        // signal for every memory it surfaced. (Failures currently go
        // unrecorded — an errored run returns Err before this point.)
        if !memory_hits.is_empty() {
            if let Some(root) = &self.config.memory_root {
                if let Ok(store) =
                    crate::memory::MemoryStore::for_workspace(root, &self.config.working_dir)
                {
                    store.record_outcome(&memory_hits, true);
                }
            }
        }

        // Terminal trace record: the run's stop label lands in the log
        // (replay/audit material — the minimal-trace-record contract).
        let _ = self
            .store
            .append(
                session_id,
                &SessionEvent::TurnTrace {
                    turn: turns,
                    harness_version: env!("CARGO_PKG_VERSION").to_string(),
                    provider: String::new(),
                    model: String::new(),
                    prompt_hash: None,
                    latency_ms: None,
                    cost_usd: None,
                    usage: None,
                    verifier: None,
                    failure_label: Some(stop_label.to_string()),
                },
            )
            .await;
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
        let system = match &self.config.memory_root {
            Some(root) => context::with_user_memory(
                &system,
                &context::load_user_memory(root, &self.config.working_dir),
            ),
            None => system,
        };
        match &self.config.memory_root {
            Some(root) => context::with_global_memory(&system, &context::load_global_memory(root)),
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
    /// Compaction trigger against an effective cap (Phase 5i): the selected
    /// model's window, not the global config cap.
    fn should_compact_with_cap(
        &self,
        turns: u32,
        turns_since_compaction: u32,
        context_tokens: u64,
        effective_cap: u64,
    ) -> bool {
        let policy = &self.config.compaction;
        policy.enabled
            && turns > 0
            && turns_since_compaction >= policy.min_interval_turns
            && (context_tokens as f64) > policy.threshold_frac * (effective_cap as f64)
    }

    /// Pre-flight estimate of the tokens the next provider call will see:
    /// system prompt + tool schemas + full transcript (incl. tool-call JSON).
    /// Run one finish-gate check at a natural end-of-run.
    ///
    /// Returns `true` when the run must CONTINUE (verification failed and
    /// rounds remain — the failure output was appended to `transcript` as a
    /// user message and persisted), `false` when the run may end. The gate
    /// disarms (`mutated` cleared) on a passing verification, and is skipped
    /// when the previous turn already ended with a passing `verify` call (no
    /// duplicate suite runs). Harness-initiated: direct tool execution, no
    /// approval prompt or hook veto; round bound is per-run (resume grants a
    /// fresh bound).
    async fn run_finish_gate(
        &self,
        session_id: SessionId,
        transcript: &mut Vec<Message>,
        mutated: &mut bool,
        rounds: &mut u32,
        answer: &mut String,
        last_dispatch_ended_with_passing_verify: bool,
    ) -> Result<bool> {
        if !*mutated || *rounds >= MAX_VERIFY_ROUNDS {
            if *mutated && *rounds >= MAX_VERIFY_ROUNDS {
                // Bound exhausted: let the run end, but never silently.
                *answer = format!(
                    "{answer}\n\n[verification still failing after {MAX_VERIFY_ROUNDS} rounds — see transcript]"
                )
                .trim()
                .to_string();
            }
            return Ok(false);
        }
        let Some(verify_tool) = self.tools.get("verify") else {
            return Ok(false);
        };
        // Skip when the previous dispatch already ended with a passing
        // verify (the model showed its evidence — no duplicate suite run).
        if last_dispatch_ended_with_passing_verify {
            *mutated = false;
            return Ok(false);
        }
        *rounds += 1;
        let ctx = ToolCtx {
            sandbox: self.config.sandbox,
            working_dir: self.config.working_dir.clone(),
            engine: Some(Arc::new(self.clone())),
        };
        let outcome = verify_tool
            .execute(serde_json::json!({}), &ctx)
            .await
            .unwrap_or_else(|err| ToolOutcome::err(err.to_string()));
        tracing::info!(
            round = *rounds,
            passed = !outcome.is_error,
            "finish gate verification"
        );
        let passed = !outcome.is_error;
        // Persisted as a user message (not an orphan ToolResult): resume and
        // compaction rebuild transcripts from the event log and an unpaired
        // Role::Tool breaks provider request building.
        let feedback = Message::user(if passed {
            format!("Verification passed:\n{}", outcome.content)
        } else {
            format!(
                "The verification command failed:\n{}\nFix the failures, then finish — the harness will re-run verification.",
                outcome.content
            )
        });
        self.store
            .append(
                session_id,
                &SessionEvent::Message {
                    parent_id: None,
                    message: feedback.clone(),
                },
            )
            .await?;
        transcript.push(feedback);
        if passed {
            *mutated = false;
            return Ok(false);
        }
        Ok(true)
    }

    /// Execute one model tool call, reusing a speculative result computed
    /// mid-stream when (tool, args) match exactly. The deny list is
    /// re-checked here: a result speculated before the dispatch must never
    /// smuggle a denied tool's output into the transcript.
    async fn dispatch_tool_call(
        &self,
        session_id: SessionId,
        call: &ToolCall,
        ctx: &ToolCtx,
        speculative: &std::collections::HashMap<String, ToolOutcome>,
    ) -> ToolOutcome {
        if SPECULATIVE_TOOLS.contains(&call.name.as_str())
            && !self.config.approvals.deny.iter().any(|d| d == &call.name)
        {
            if let Some(outcome) = speculative.get(&speculative_key(call)) {
                tracing::info!(tool = %call.name, "speculative hit: reuse mid-stream result");
                return outcome.clone();
            }
        }
        self.execute_tool_with_hooks(session_id, call, ctx)
            .await
            .unwrap_or_else(|err| ToolOutcome::err(err.to_string()))
    }

    /// One compaction pass (X1, PLAN.md §4.10): load the session's events,
    /// render everything older than the policy tail
    /// ([`compact::older_transcript_text`]), summarize it with a routed
    /// cheap-model call (`ActionKind::Summarize`, small output cap, no
    /// tools), cap the summary ([`compact::cap_summary`]), append the
    /// `Compaction` event plus its `Usage` event, and return the rebuilt
    /// transcript: `[compact state] + recent tail verbatim`.
    ///
    /// Bitmap compaction (additive): when the policy allows it and
    /// `next_model` — the model selected for the turn AFTER the compaction —
    /// is vision-capable, the same older-transcript text is also rendered as
    /// one pixel-font PNG and attached to the compact-state message, so the
    /// model reads the full pruned history from a single image instead of
    /// the lossy summary alone. The text summary is always kept.
    ///
    /// `Ok(None)` means there was nothing older than the tail to summarize —
    /// no provider call, no events, no interval reset. The caller feeds the
    /// returned usage into the budget governor and run totals.
    async fn run_compaction(
        &self,
        session_id: SessionId,
        task: &str,
        next_model: &str,
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
            messages: vec![Message::user(older_text.as_str())],
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
        // The bitmap (when gated in below) rides on that message only — the
        // persisted `Compaction` event stays text-shaped, so old logs and
        // non-vision resumes are unaffected.
        let mut compact_message = compact_state_message(&summary);
        if let Some(bitmap) = self.compaction_bitmap(&older_text, next_model) {
            compact_message.attachments.push(bitmap);
        }
        let mut transcript = Vec::with_capacity(tail.len() + 1);
        transcript.push(compact_message);
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

    /// The compact-state bitmap attachment, or `None` when it does not
    /// apply: the policy kill-switch is off, the next-turn model is not
    /// vision-capable (an unreadable image is pure token spend), or the
    /// older transcript misses the bitmap's caps (empty/oversize). Every
    /// refusal degrades to the text-only compact state — never an error.
    fn compaction_bitmap(&self, older_text: &str, next_model: &str) -> Option<crate::Attachment> {
        if !self.config.compaction.bitmap_compaction || !bitmap_compact::vision_capable(next_model)
        {
            tracing::debug!(
                model = %next_model,
                "compaction bitmap skipped (policy off or non-vision model)"
            );
            return None;
        }
        bitmap_compact::render_text_bitmap(older_text, bitmap_compact::COMPACT_BITMAP_WIDTH_PX)
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
/// sha256 over the exact wire bytes of (system + tool specs): the
/// prefix-stability key for cache-discipline auditing.
fn prompt_wire_hash(system: &str, tools: &[ToolSpec]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(system.as_bytes());
    if let Ok(wire) = serde_json::to_string(tools) {
        hasher.update(wire.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

fn events_to_messages<'a>(events: impl IntoIterator<Item = &'a SessionEvent>) -> Vec<Message> {
    let mut messages = Vec::new();
    for event in events {
        match event {
            SessionEvent::TurnTrace { .. } => {}
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
    use crate::{EngineConfig, Tool, ToolOutcome, ToolRegistry, TurnResponse};
    use async_trait::async_trait;
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

    /// A write tool (records the call) and a verify tool whose first run
    /// fails and second passes — the finish-gate scenario.
    struct GateWriteTool;
    #[async_trait]
    impl Tool for GateWriteTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "write".into(),
                description: "test write".into(),
                parameters: json!({"type": "object"}),
            }
        }
        async fn execute(&self, _args: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutcome> {
            Ok(ToolOutcome::ok("wrote"))
        }
    }
    struct GateVerifyTool(std::sync::atomic::AtomicUsize);
    #[async_trait]
    impl Tool for GateVerifyTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "verify".into(),
                description: "test verify".into(),
                parameters: json!({"type": "object"}),
            }
        }
        async fn execute(&self, _args: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutcome> {
            let n = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n == 0 {
                Ok(ToolOutcome::err("test run failed: 2 tests failed"))
            } else {
                Ok(ToolOutcome::ok("all green"))
            }
        }
    }

    struct AlwaysFailVerifyTool;
    #[async_trait]
    impl Tool for AlwaysFailVerifyTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "verify".into(),
                description: "test verify".into(),
                parameters: json!({"type": "object"}),
            }
        }
        async fn execute(&self, _args: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutcome> {
            Ok(ToolOutcome::err("test run failed"))
        }
    }

    fn gate_engine(script: Vec<TurnResponse>) -> (Engine, std::sync::Arc<GateVerifyTool>) {
        use std::collections::HashMap;
        let mut registry = ToolRegistry::new();
        registry.register(std::sync::Arc::new(GateWriteTool));
        let verify = std::sync::Arc::new(GateVerifyTool(std::sync::atomic::AtomicUsize::new(0)));
        registry.register(std::sync::Arc::clone(&verify) as std::sync::Arc<dyn Tool>);
        let mut providers: HashMap<String, std::sync::Arc<dyn ModelProvider>> = HashMap::new();
        providers.insert(
            "mock".into(),
            std::sync::Arc::new(crate::testing::MockProvider::with_script(script)),
        );
        let engine = Engine {
            selector: std::sync::Arc::new(crate::testing::StaticSelector::new("m", "mock")),
            providers,
            tools: std::sync::Arc::new(registry),
            store: std::sync::Arc::new(crate::testing::InMemoryStore::default()),
            config: EngineConfig::default(),
        };
        (engine, verify)
    }

    fn call(name: &str) -> TurnResponse {
        TurnResponse {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: format!("c-{name}"),
                name: name.into(),
                arguments: json!({}),
            }],
            usage: Default::default(),
            stop_reason: StopReason::ToolUse,
        }
    }

    fn text(text: &str) -> TurnResponse {
        TurnResponse {
            text: text.into(),
            tool_calls: Vec::new(),
            usage: Default::default(),
            stop_reason: StopReason::EndTurn,
        }
    }

    #[tokio::test]
    async fn finish_gate_feeds_verify_failures_back_until_green() {
        // Turn 1 writes; turn 2 tries to finish (verify FAILS -> feedback);
        // turn 3 finishes again (verify passes -> run ends).
        let (engine, verify) = gate_engine(vec![call("write"), text("done?"), text("done")]);
        let summary = engine.run_task("ship it").await.unwrap();
        assert_eq!(summary.answer, "done");
        assert_eq!(
            verify.0.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "verify ran twice (fail then pass)"
        );
        assert!(summary.turns >= 3, "the gate forced an extra model turn");
    }

    #[tokio::test]
    async fn turn_trace_records_land_in_the_session_log() {
        let (engine, _verify) = gate_engine(vec![text("done")]);
        let summary = engine.run_task("hello").await.unwrap();
        let events = engine.store.load(summary.session_id).await.unwrap();
        let traces: Vec<(u32, Option<String>, Option<String>)> = events
            .iter()
            .filter_map(|e| match e {
                SessionEvent::TurnTrace {
                    turn,
                    prompt_hash,
                    failure_label,
                    ..
                } => Some((*turn, prompt_hash.clone(), failure_label.clone())),
                _ => None,
            })
            .collect();
        assert!(
            traces.iter().any(|(t, hash, _)| *t == 1 && hash.is_some()),
            "per-turn record carries the prompt hash: {traces:?}"
        );
        assert!(
            traces
                .iter()
                .any(|(_, _, label)| label.as_deref() == Some("end_turn")),
            "terminal record carries the stop label: {traces:?}"
        );
    }

    /// Glob stub counting executions (speculative-execution tests).
    struct CountingGlob(Arc<std::sync::atomic::AtomicUsize>);
    #[async_trait]
    impl Tool for CountingGlob {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "glob".into(),
                description: "counting glob".into(),
                parameters: json!({"type": "object"}),
            }
        }
        async fn execute(&self, _args: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutcome> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            Ok(ToolOutcome::ok("one file"))
        }
    }

    /// A provider that completes a read tool call MID-STREAM (emitting
    /// ToolCallReady) and returns the same call at turn end.
    struct SpeculativeStreamProvider {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }
    #[async_trait]
    impl ModelProvider for SpeculativeStreamProvider {
        fn id(&self) -> &str {
            "spec-stream"
        }
        async fn turn(&self, _req: TurnRequest) -> Result<TurnResponse> {
            unreachable!("streaming path under test")
        }
        async fn turn_streaming(
            &self,
            _req: TurnRequest,
            tx: tokio::sync::mpsc::UnboundedSender<StreamDelta>,
        ) -> Result<TurnResponse> {
            if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0 {
                // Turn 2+: finish with plain text.
                let _ = tx.send(StreamDelta::Text("done".into()));
                return Ok(TurnResponse {
                    text: "done".into(),
                    tool_calls: Vec::new(),
                    usage: Default::default(),
                    stop_reason: StopReason::EndTurn,
                });
            }
            let read_call = ToolCall {
                id: "s1".into(),
                name: "glob".into(),
                arguments: json!({"pattern": "*.rs"}),
            };
            let _ = tx.send(StreamDelta::Text("checking ".into()));
            let _ = tx.send(StreamDelta::ToolCallReady(read_call.clone()));
            // Simulate the rest of the turn still streaming.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            Ok(TurnResponse {
                text: String::new(),
                tool_calls: vec![read_call],
                usage: Default::default(),
                stop_reason: StopReason::ToolUse,
            })
        }
    }

    #[tokio::test]
    async fn speculation_disabled_when_a_speculative_tool_is_denied() {
        use std::collections::HashMap;
        let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register(std::sync::Arc::new(CountingGlob(Arc::clone(&executions))));
        let mut providers: HashMap<String, Arc<dyn ModelProvider>> = HashMap::new();
        providers.insert(
            "spec-stream".into(),
            Arc::new(SpeculativeStreamProvider {
                calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }),
        );
        let engine = Engine {
            selector: Arc::new(crate::testing::StaticSelector::new("m", "spec-stream")),
            providers,
            tools: Arc::new(registry),
            store: Arc::new(crate::testing::InMemoryStore::default()),
            config: EngineConfig {
                on_delta: Some(Arc::new(|_delta| {})),
                approvals: crate::ApprovalPolicy {
                    deny: vec!["glob".into()],
                    ..crate::ApprovalPolicy::default()
                },
                ..EngineConfig::default()
            },
        };
        // glob is denied: the dispatch blocks the call, so the run ends
        // without any glob execution — and crucially the speculative
        // driver must NOT have executed it either.
        let summary = engine.run_task("list files").await.unwrap();
        assert_eq!(
            executions.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a denied tool must never execute — not even speculatively"
        );
        // The denied call still counts as dispatched (its blocked outcome
        // enters the transcript) — one call, zero executions is the contract.
        assert_eq!(summary.tool_calls, 1);
    }

    #[tokio::test]
    async fn speculative_read_runs_once_and_is_reused_at_dispatch() {
        use std::collections::HashMap;
        let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register(std::sync::Arc::new(CountingGlob(Arc::clone(&executions))));
        let mut providers: HashMap<String, Arc<dyn ModelProvider>> = HashMap::new();
        providers.insert(
            "spec-stream".into(),
            Arc::new(SpeculativeStreamProvider {
                calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }),
        );
        let engine = Engine {
            selector: Arc::new(crate::testing::StaticSelector::new("m", "spec-stream")),
            providers,
            tools: Arc::new(registry),
            store: Arc::new(crate::testing::InMemoryStore::default()),
            // Streaming + an on_delta sink arms the speculative driver.
            config: EngineConfig {
                on_delta: Some(Arc::new(|_delta| {})),
                ..EngineConfig::default()
            },
        };
        let summary = engine.run_task("list files").await.unwrap();
        assert!(summary.tool_calls >= 1, "the turn dispatched the glob call");
        assert_eq!(
            executions.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "glob executed exactly once (speculated mid-stream, reused at dispatch)"
        );
    }

    #[tokio::test]
    async fn finish_gate_is_inert_without_a_registered_verify_tool() {
        use std::collections::HashMap;
        let mut registry = ToolRegistry::new();
        registry.register(std::sync::Arc::new(GateWriteTool));
        let mut providers: HashMap<String, std::sync::Arc<dyn ModelProvider>> = HashMap::new();
        providers.insert(
            "mock".into(),
            std::sync::Arc::new(crate::testing::MockProvider::with_script(vec![
                call("write"),
                text("done"),
            ])),
        );
        let engine = Engine {
            selector: std::sync::Arc::new(crate::testing::StaticSelector::new("m", "mock")),
            providers,
            tools: std::sync::Arc::new(registry),
            store: std::sync::Arc::new(crate::testing::InMemoryStore::default()),
            config: EngineConfig::default(),
        };
        let summary = engine.run_task("ship it").await.unwrap();
        assert_eq!(summary.turns, 2, "no verify tool registered: no gate");
    }

    #[tokio::test]
    async fn finish_gate_gives_up_after_max_rounds() {
        // Verify always fails: the run ends after MAX_VERIFY_ROUNDS rounds
        // of feedback rather than looping forever.
        use std::collections::HashMap;
        let mut registry = ToolRegistry::new();
        registry.register(std::sync::Arc::new(GateWriteTool));
        registry.register(std::sync::Arc::new(AlwaysFailVerifyTool));
        let mut providers: HashMap<String, std::sync::Arc<dyn ModelProvider>> = HashMap::new();
        providers.insert(
            "mock".into(),
            std::sync::Arc::new(crate::testing::MockProvider::with_script(vec![
                call("write"),
                text("attempt 1"),
                text("attempt 2"),
                text("attempt 3"),
            ])),
        );
        let engine = Engine {
            selector: std::sync::Arc::new(crate::testing::StaticSelector::new("m", "mock")),
            providers,
            tools: std::sync::Arc::new(registry),
            store: std::sync::Arc::new(crate::testing::InMemoryStore::default()),
            config: EngineConfig::default(),
        };
        let summary = engine.run_task("ship it").await.unwrap();
        assert_eq!(
            summary.answer,
            "attempt 3

[verification still failing after 2 rounds — see transcript]"
        );
    }
}
