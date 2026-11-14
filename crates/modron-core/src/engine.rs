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
use crate::speculative;
use crate::{
    estimate_tokens, judge, ActionKind, ApprovalMode, Engine, Error, JudgeMode, Message,
    ModelChoice, ModelProvider, Result, Role, RunSummary, SelectRequest, SessionEvent, SessionId,
    StopReason, StreamDelta, ToolActivity, ToolCall, ToolCtx, ToolOutcome, ToolSpec, TurnRequest,
    TurnResponse, Usage,
};
use std::sync::Arc;

/// Output-token ask sent with every provider turn (`EngineConfig` has no
/// output cap; this is a sane non-streaming default).
const MAX_OUTPUT_TOKENS: u32 = 8192;

/// The approval-gated tool class (A1, decision D1): these change system state
/// or run model-supplied commands, so they need an explicit approval outside
/// `ApproveAll`. Every tool whose name starts with `mcp__` is gated too (see
/// [`is_gated`]). `remember` and the read-only tools stay ungated.
///
/// Every new tool needs an explicit gated/ungated decision here: a tool that
/// is not added to this list runs without approval.
const MUTATING_TOOLS: [&str; 7] = [
    "bash",
    "write",
    "edit",
    "verify",
    "diagnostics",
    "interact",
    "delegate",
];

/// The file-mutating subset of the approval-gated class. Finish-gate arming
/// and pre-mutation checkpoints key off this list, not [`MUTATING_TOOLS`]:
/// widening the approval gate must not change their behavior.
const FILE_MUTATING_TOOLS: [&str; 4] = ["bash", "write", "edit", "verify"];

/// Accumulate provider-reported usage into a run total. Counts come from
/// the provider (possibly hostile or buggy), so every field saturates
/// instead of wrapping (release) or panicking (debug).
fn add_usage(total: &mut Usage, delta: &Usage) {
    total.input_tokens = total.input_tokens.saturating_add(delta.input_tokens);
    total.output_tokens = total.output_tokens.saturating_add(delta.output_tokens);
    total.cached_input_tokens = total
        .cached_input_tokens
        .saturating_add(delta.cached_input_tokens);
}

/// True when `message` starts with the canonical HTTP-layer status prefix
/// `HTTP <code>` followed by a space or `:` (the shape
/// `modron-providers` produces as `HTTP <status>: <snippet>`). Anything else,
/// including a provider body that merely mentions the code, is not a match.
fn has_http_status_prefix(message: &str, code: &str) -> bool {
    message
        .strip_prefix("HTTP ")
        .and_then(|rest| rest.strip_prefix(code))
        .is_some_and(|tail| tail.starts_with(' ') || tail.starts_with(':'))
}

/// Whether a tool call needs approval before it runs (A1, D1): any name in
/// [`MUTATING_TOOLS`] or any `mcp__`-prefixed tool.
fn is_gated(name: &str) -> bool {
    MUTATING_TOOLS.contains(&name) || name.starts_with("mcp__")
}

/// The approval policy's verdict on one tool name, before any prompt (A5).
/// Model-issued calls and harness-initiated calls (the finish gate's verify)
/// both go through [`Engine::admission`], so the rules live in one place.
enum Admission {
    /// Runs without asking: ungated, allow-listed, or ApproveAll.
    Run,
    /// Needs an interactive decision: Ask mode with an approver installed.
    Ask,
    /// Refused outright; the payload is the error the model (or the gate)
    /// sees.
    Refused(String),
}

/// The block message for an approval mode that will not run a gated call.
fn approval_block_hint(mode: ApprovalMode) -> String {
    match mode {
        ApprovalMode::Deny => "blocked by approval mode: deny".to_string(),
        _ => "blocked by approval mode: ask (non-interactive run — rerun with \
              --approve-all to allow mutating tools)"
            .to_string(),
    }
}

/// Largest file whose prior bytes a checkpoint captures (D8).
const SNAPSHOT_CAP: u64 = 256 * 1024;

/// The state of a file before a checkpointed mutation (D8).
#[derive(Debug, PartialEq, Eq)]
enum PriorState {
    /// The file did not exist: rewind deletes what the tool creates.
    Absent,
    /// The file's bytes, base64-encoded: rewind restores them.
    Captured(String),
    /// The file exists but cannot be captured (not a regular file, over the
    /// cap, or unreadable): rewind refuses and changes nothing.
    Uncaptured,
}

/// Decide a file's prior state. The file type and size are checked with
/// `symlink_metadata` BEFORE any read, so a FIFO, device or huge file is
/// never opened for content.
fn prior_state(path: &std::path::Path) -> PriorState {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return PriorState::Absent,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "cannot stat file for checkpoint");
            return PriorState::Uncaptured;
        }
    };
    if !meta.file_type().is_file() {
        tracing::warn!(path = %path.display(), "checkpoint skipped: not a regular file");
        return PriorState::Uncaptured;
    }
    if meta.len() > SNAPSHOT_CAP {
        tracing::warn!(
            path = %path.display(),
            bytes = meta.len(),
            "file exceeds 256KB checkpoint cap — rewind cannot restore it"
        );
        return PriorState::Uncaptured;
    }
    match std::fs::read(path) {
        Ok(bytes) if bytes.len() as u64 <= SNAPSHOT_CAP => {
            PriorState::Captured(crate::b64::encode(&bytes))
        }
        Ok(_) => PriorState::Uncaptured,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "cannot read file for checkpoint");
            PriorState::Uncaptured
        }
    }
}

/// Resolve a tool's `path` argument to the real location it would touch,
/// following symlinks on the part that exists. Returns `None` unless that
/// location is inside `working_dir` (A3). The `..` components are resolved
/// by the kernel through the existing prefix, never folded lexically, so a
/// symlink followed by `..` cannot make a path look inside.
fn resolve_inside(working_dir: &std::path::Path, path: &str) -> Option<std::path::PathBuf> {
    use std::path::{Component, Path, PathBuf};
    let base = std::fs::canonicalize(working_dir).ok()?;
    let raw = Path::new(path);
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        base.join(raw)
    };
    // Walk up to the longest existing ancestor. Components that do not
    // exist yet must be plain names: a `..` in that tail is refused.
    let mut existing: &Path = joined.as_path();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while std::fs::symlink_metadata(existing).is_err() {
        match existing.components().next_back() {
            Some(Component::Normal(name)) => tail.push(name.to_os_string()),
            _ => return None,
        }
        existing = existing.parent()?;
    }
    let mut resolved: PathBuf = std::fs::canonicalize(existing).ok()?;
    for name in tail.iter().rev() {
        resolved.push(name);
    }
    resolved.starts_with(&base).then_some(resolved)
}

/// The events a resumed run may see after a rewind. No cut: everything.
/// Cut only: `events[..cut]`. Cut and end: `events[..cut]` followed by
/// `events[end..]`, i.e. only the abandoned range `[cut, end)` is skipped.
fn rewind_view<'a>(
    events: &'a [SessionEvent],
    cut: Option<u64>,
    end: Option<u64>,
    earlier: &[(u64, u64)],
) -> std::borrow::Cow<'a, [SessionEvent]> {
    use std::borrow::Cow;
    let len = events.len();
    let clamp = |v: u64| usize::try_from(v).unwrap_or(usize::MAX).min(len);
    // Every abandoned range as [start, end) in event indices. The current
    // marker without an end keeps the old meaning: drop to the log end.
    let mut ranges: Vec<(usize, usize)> = earlier
        .iter()
        .map(|&(start, end)| (clamp(start), clamp(end)))
        .collect();
    if let Some(cut) = cut {
        let start = clamp(cut);
        ranges.push((start, end.map_or(len, clamp).max(start)));
    }
    ranges.retain(|&(start, end)| start < end);
    match ranges.as_slice() {
        [] => Cow::Borrowed(events),
        // One range reaching the log end: a plain prefix, no copy.
        [(start, end)] if *end == len => Cow::Borrowed(&events[..*start]),
        _ => Cow::Owned(
            events
                .iter()
                .enumerate()
                .filter(|(idx, _)| !ranges.iter().any(|&(s, e)| (s..e).contains(idx)))
                .map(|(_, event)| event.clone())
                .collect(),
        ),
    }
}

/// Finish-gate bound: verification feedback rounds before the run may end
/// without passing verification (StaminaBench: gains plateau after 3-5
/// retries; 2 keeps the worst case tight).
const MAX_VERIFY_ROUNDS: u32 = 2;

/// System-prompt addendum for the escalation turn (verify-failure
/// escalation): one appended block, not a rebuilt prompt, so the escalated
/// model sees the same identity/tool contract the run used and only the
/// mission changes. Byte-stable: tests match on it, and its presence in a
/// captured request is what marks a turn as the escalation.
const ESCALATION_SYSTEM_ADDENDUM: &str = "\n\nVERIFICATION ESCALATION: the \
run's automated verification failed and the normal feedback rounds were \
exhausted. You are the escalation reviewer: re-read the ORIGINAL task and \
re-check EVERY concrete requirement it names (file paths, exact names, \
passwords, ports, config file locations and formats, expected content) \
against the evidence in this conversation. Your next message REPLACES the \
run's final answer — correct any mismatch you find and state plainly \
anything you cannot confirm.";

/// Output-token ask for the compaction (summarizer) call — small by design
/// (PLAN.md §4.10): a compact state is a few hundred tokens at most, and the
/// summary is byte-capped again before it is persisted.
const COMPACTION_MAX_OUTPUT_TOKENS: u32 = 512;

/// Characters of the task used as the session title.
const TITLE_MAX_CHARS: usize = 60;

// ---------------------------------------------------------------------------
// Run-loop phase types (2026-11-12 audit): the per-run mutable locals the
// turn loop and its helpers previously shared as ~15 loose locals plus five
// separate `&mut` parameters are gathered into [`RunLoopState`]; the loop
// body is split into route / execute / dispatch / finalize phases.
// ---------------------------------------------------------------------------

/// Routing decision for one turn — the output of the route/decide phase
/// ([`Engine::route_turn`]) and the input to the execute-turn phase.
struct TurnRoute {
    choice: ModelChoice,
    context_tokens: u64,
    action: ActionKind,
}

/// One completed provider turn plus the bookkeeping its caller needs:
/// accounting inputs (model, provider id, start instant) and the
/// speculative mid-stream results the dispatch reuses.
struct CompletedTurn {
    response: TurnResponse,
    model: String,
    provider_id: String,
    /// When this turn's provider call started (the TurnTrace latency).
    started: std::time::Instant,
    /// Speculative results computed during THIS turn's stream, keyed by
    /// (tool, exact wire args).
    speculative: std::collections::HashMap<String, ToolOutcome>,
}

/// Per-run mutable state for one [`Engine::run_loop`] execution.
struct RunLoopState {
    /// Conversation so far: rebuilt by compaction, extended by every
    /// assistant message and tool result.
    transcript: Vec<Message>,
    /// Cumulative reported-usage budget for the whole run.
    governor: BudgetGovernor,
    turns: u32,
    tool_calls_executed: u32,
    /// Provider-reported ground-truth usage totals for the run summary.
    total: Usage,
    models_used: Vec<String>,
    /// The run's final answer: annotated by the finish gate's give-up note,
    /// replaced by the escalation turn on success.
    answer: String,
    /// Main turns completed since the last compaction (the compaction call
    /// itself is not a turn).
    turns_since_compaction: u32,
    // Phase 13: memory ids this run surfaced via memory_search — folded
    // back into the usefulness ledger when the run completes.
    memory_hits: Vec<String>,
    spend_usd: f64,
    /// Finish-gate state: whether a mutating tool was dispatched this
    /// run, and how many verification rounds have been spent.
    mutated_this_run: bool,
    verify_rounds: u32,
    /// Whether the finish gate already injected its self-check turn. The
    /// self-check runs at most once per run.
    self_check_sent: bool,
    /// Set when the self-check message is injected, cleared by the next
    /// provider turn. Only that turn is non-fatal on a provider error (keep
    /// the answer); a provider error on any later turn propagates (A6).
    self_check_turn_pending: bool,
    /// Tier fallback (remote-control hardening): once an auth failure
    /// promotes the run to a tier's fallback model, every later turn
    /// uses the fallback directly (the dead primary would 401 again
    /// each turn). `(primary model, fallback selection)`.
    run_tier_override: Option<(String, ModelChoice)>,
    /// Whether the most recent dispatch turn ENDED with a passing
    /// `verify` call — the model's own evidence satisfies the gate.
    last_dispatch_ended_with_passing_verify: bool,
    /// Why the loop ended (`end_turn` / `max_turns` / `spend_cap` /
    /// `budget_exhausted`) — the terminal TurnTrace's failure label.
    stop_label: &'static str,
}

impl RunLoopState {
    fn new(transcript: Vec<Message>, max_total_tokens: u64) -> Self {
        Self {
            transcript,
            governor: BudgetGovernor::new(max_total_tokens),
            turns: 0,
            tool_calls_executed: 0,
            total: Usage::default(),
            models_used: Vec::new(),
            answer: String::new(),
            turns_since_compaction: 0,
            memory_hits: Vec::new(),
            spend_usd: 0.0,
            mutated_this_run: false,
            verify_rounds: 0,
            self_check_sent: false,
            self_check_turn_pending: false,
            run_tier_override: None,
            last_dispatch_ended_with_passing_verify: false,
            stop_label: "end_turn",
        }
    }
}

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
        // With `resume_skip_end` only the abandoned range `[cut, end)` is
        // dropped, so work appended after the rewind survives later resumes.
        let kept = rewind_view(
            &events,
            self.config.resume_from_event,
            self.config.resume_skip_end,
            &self.config.resume_skip_ranges,
        );
        let sliced: &[SessionEvent] = &kept;
        // Every resume goes through the prune, not only rewind resumes: an
        // interrupt mid-tool leaves an assistant tool_use with no result in
        // the log, and both provider APIs reject that (cycle-2 audit H1).
        // The cut (if any) lands before abandoned results, so the same
        // rewrite also trims a rewind tail.
        let pruned = prune_unanswered_tool_calls(sliced);
        let mut transcript = reconstruct_transcript(&pruned, self.config.compaction.tail_messages);
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
    ///
    /// Phase shape (2026-11-12 audit): each iteration routes/decides
    /// ([`Self::route_turn`]), executes one provider turn
    /// ([`Self::execute_provider_turn`]), then either dispatches tool calls
    /// ([`Self::dispatch_turn_tools`]) or runs the finish gate inline;
    /// [`Self::finalize_run`] closes the run. Per-run mutable state lives in
    /// [`RunLoopState`].
    async fn run_loop(
        &self,
        session_id: SessionId,
        task: &str,
        transcript: Vec<Message>,
    ) -> Result<RunSummary> {
        let tool_specs = self.tools.specs_prompt();
        let system = self.assemble_system(&tool_specs);
        // Cache-discipline audit key: one hash over the exact wire prefix
        // (system + tool specs) — a change across turns means prefix
        // instability (cache busts), visible in TurnTrace records.
        let prompt_hash = prompt_wire_hash(&system, &tool_specs);
        let mut state = RunLoopState::new(transcript, self.config.max_total_tokens);

        loop {
            if state.turns >= self.config.max_turns {
                tracing::warn!(
                    turns = state.turns,
                    max = self.config.max_turns,
                    "stopping: max_turns reached"
                );
                state.stop_label = "max_turns";
                break;
            }

            // Route FIRST (Phase 5i): the selected model's context window
            // (from ModelChoice) is the compaction trigger — a 128k-window
            // free model must compact at 0.8×128k even when the global cap
            // is 180k.
            let route = self
                .route_turn(session_id, task, &system, &tool_specs, &mut state)
                .await?;

            // Loop-external turn state: `None` = self-check short-circuit
            // (the outer run loop ends with the preserved answer).
            let Some(turn) = self
                .execute_provider_turn(session_id, &system, &tool_specs, &route, &mut state)
                .await?
            else {
                break;
            };
            state.turns += 1;

            // Usage: provider-reported ground truth, never estimates.
            state.governor.feed_turn(turn.response.usage);
            add_usage(&mut state.total, &turn.response.usage);
            self.store
                .append(
                    session_id,
                    &SessionEvent::Usage {
                        model: turn.model.clone(),
                        usage: turn.response.usage,
                    },
                )
                .await?;
            if !state.models_used.contains(&turn.model) {
                state.models_used.push(turn.model.clone());
            }
            let turn_cost = self.turn_cost_usd(&turn.model, &turn.response.usage);
            state.spend_usd += turn_cost;
            self.store
                .append(
                    session_id,
                    &SessionEvent::TurnTrace {
                        turn: state.turns,
                        harness_version: env!("CARGO_PKG_VERSION").to_string(),
                        provider: turn.provider_id.clone(),
                        model: turn.model.clone(),
                        prompt_hash: Some(prompt_hash.clone()),
                        latency_ms: Some(turn.started.elapsed().as_millis() as u64),
                        cost_usd: Some(turn_cost),
                        usage: Some(turn.response.usage),
                        verifier: None,
                        failure_label: None,
                    },
                )
                .await?;

            // USD spend cap (A3): computed from provider-reported usage and
            // the injected price lookup — an honest estimate, checked after
            // the turn so the turn's real usage is what trips it.
            if let Some(cap) = self.config.max_cost_usd {
                if state.spend_usd > cap {
                    tracing::error!(
                        spend = format!("${:.4}", state.spend_usd),
                        cap = format!("${cap:.4}"),
                        "stopping: USD spend cap exceeded"
                    );
                    // The capped turn's own text is the run's final answer;
                    // keep it (not the previous turn's) ahead of the note.
                    state.answer = turn.response.text.clone();
                    state.answer = format!(
                        "{}\n\n[stopped: USD spend cap exceeded — spent ~${:.4} of ${cap:.4} cap]",
                        state.answer, state.spend_usd
                    )
                    .trim()
                    .to_string();
                    state.stop_label = "spend_cap";
                    break;
                }
            }

            // Record the assistant message (text and/or tool calls).
            let assistant = Message {
                role: Role::Assistant,
                content: turn.response.text.clone(),
                tool_calls: turn.response.tool_calls.clone(),
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
            state.transcript.push(assistant);
            state.answer = turn.response.text;

            if turn.response.stop_reason != StopReason::ToolUse
                || turn.response.tool_calls.is_empty()
            {
                // Finish gate (harness-enforced verification): a run that
                // dispatched mutating tools must show verify evidence
                // before it may end — outcome-only scoring hides unverified
                // work (AgentLens: 94.3% of lucky passes lack it). Arms
                // only when a `verify` tool is registered (i.e. [verify]
                // configured); failures feed the output back verbatim and
                // continue — StaminaBench's retry-with-test-feedback loop,
                // bounded to MAX_VERIFY_ROUNDS per run.
                if !self.run_finish_gate(session_id, &mut state).await? {
                    // Escalation (ONE per run): the gate just exhausted its
                    // feedback rounds with verification still failing (the
                    // same two conditions its give-up note keys on) and the
                    // config names an escalation model. The escalated turn's
                    // answer replaces `answer` on success; any failure is
                    // swallowed inside (warn) and the give-up note survives.
                    // The bound needs no counter: this branch always exits
                    // the loop below, so the site — and thus the escalation
                    // — can fire at most once per run (a resume grants a
                    // fresh bound, like the gate rounds). Max-turns headroom
                    // is required: the persisted handoff message must not
                    // promise a re-check the loop can no longer service.
                    if state.mutated_this_run
                        && state.verify_rounds >= MAX_VERIFY_ROUNDS
                        && state.turns < self.config.max_turns
                        && self.config.escalate_on_check_failure.is_some()
                    {
                        if let Err(err) = self
                            .run_escalation_turn(session_id, task, &system, &mut state)
                            .await
                        {
                            // Store-level failures degrade too: a completed
                            // run with the old answer beats losing it to an
                            // escalation bookkeeping error.
                            tracing::warn!(
                                error = %err,
                                "escalation turn failed; keeping the pre-escalation answer"
                            );
                        }
                    }
                    break;
                }
                continue;
            }

            // Dispatch the tool calls (gate arming, hook pipeline, result
            // bookkeeping — see [`Self::dispatch_turn_tools`]).
            self.dispatch_turn_tools(
                session_id,
                &turn.response.tool_calls,
                &turn.model,
                &turn.speculative,
                &mut state,
            )
            .await?;

            // A main turn completed: it counts toward the compaction
            // min-interval (the compaction call itself is not a turn).
            state.turns_since_compaction += 1;

            if state.governor.exceeded() {
                tracing::warn!(
                    cumulative = state.governor.cumulative(),
                    cap = self.config.max_total_tokens,
                    "stopping: cumulative token budget exceeded"
                );
                state.stop_label = "budget_exhausted";
                break;
            }
        }

        self.finalize_run(session_id, &mut state).await
    }

    /// Route/decide phase of one loop iteration (Phase 5i): estimate the
    /// context, select the model, maybe compact first — the selected
    /// model's context window is the compaction trigger — pre-flight the
    /// budget cap against that window, and consult the async decision
    /// model. No session events are emitted here (compaction's own
    /// excepted); the next provider turn appends `ModelSelected`.
    async fn route_turn(
        &self,
        session_id: SessionId,
        task: &str,
        system: &str,
        tool_specs: &[ToolSpec],
        state: &mut RunLoopState,
    ) -> Result<TurnRoute> {
        let mut context_tokens = estimate_context_tokens(system, tool_specs, &state.transcript);
        let action = if state.turns == 0 {
            ActionKind::Chat
        } else {
            derive_action(&state.transcript)
        };
        let mut choice = self.selector.select(&SelectRequest {
            task: task.to_string(),
            action,
            context_tokens,
            budget_remaining_usd: self
                .config
                .max_cost_usd
                .map(|cap| (cap - state.spend_usd).max(0.0)),
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
            state.turns,
            state.turns_since_compaction,
            context_tokens,
            effective_cap,
        ) {
            if let Some(outcome) = self.run_compaction(session_id, task, &choice.model).await? {
                state.governor.feed_turn(outcome.usage);
                state.spend_usd += self.turn_cost_usd(&outcome.model, &outcome.usage);
                add_usage(&mut state.total, &outcome.usage);
                if !state.models_used.contains(&outcome.model) {
                    state.models_used.push(outcome.model);
                }
                state.transcript = outcome.transcript;
                state.turns_since_compaction = 0;
                context_tokens = estimate_context_tokens(system, tool_specs, &state.transcript);
                // The context changed materially: re-route once.
                choice = self.selector.select(&SelectRequest {
                    task: task.to_string(),
                    action,
                    context_tokens,
                    budget_remaining_usd: self
                        .config
                        .max_cost_usd
                        .map(|cap| (cap - state.spend_usd).max(0.0)),
                });
            }
        }

        // Pre-flight budget check on the assembled context, against the
        // effective cap (the model's window when known).
        state
            .governor
            .check_input_with_cap(context_tokens, effective_cap)?;

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
                            .map(|cap| (cap - state.spend_usd).max(0.0)),
                    };
                    let mut re = self.selector.select(&synthetic);
                    re.reason = format!("async-router: {verdict:?} → {}", re.reason);
                    choice = re;
                }
            }
        }
        Ok(TurnRoute {
            choice,
            context_tokens,
            action,
        })
    }

    /// Execute-turn phase: resolve the provider per the route (with the
    /// tier-fallback retry ONCE on auth-class failures), drive the provider
    /// call — streaming, with the speculative driver armed when the config
    /// allows it — and classify the outcome:
    ///
    /// - `Ok(Some(turn))` — a completed turn with its accounting inputs and
    ///   the speculative mid-stream results for the dispatch,
    /// - `Ok(None)` — self-check short-circuit: the outer run loop ends
    ///   with the preserved answer,
    /// - `Err(e)` — the run fails (after the memory-ledger failure signal).
    async fn execute_provider_turn(
        &self,
        session_id: SessionId,
        system: &str,
        tool_specs: &[ToolSpec],
        route: &TurnRoute,
        state: &mut RunLoopState,
    ) -> Result<Option<CompletedTurn>> {
        // Tier fallback: a run-level override (an earlier turn's auth
        // failure promoted this model) applies immediately; otherwise
        // the selector's choice stands. The fallback flag mirrors the
        // override so a fresh primary failure can still be retried on
        // a candidate.
        let mut fallback: Option<ModelChoice> = match &state.run_tier_override {
            Some((primary, fb)) if *primary == route.choice.model => Some(fb.clone()),
            _ => None,
        };
        let mut attempted_fallback = fallback.is_some();
        // Consume the self-check marker: only THIS provider turn may swallow
        // a failure (A6). Set by the finish gate just before this turn.
        let self_check_turn = std::mem::take(&mut state.self_check_turn_pending);
        // Loop-external turn state: None = self-check short-circuit
        // (the outer run loop ends with the preserved answer).
        let mut turn_outcome: Option<(TurnResponse, String, String)> = None;
        #[allow(unused_assignments)]
        let mut turn_started = std::time::Instant::now();
        // Speculative results computed during THIS turn's stream.
        let mut speculative: std::collections::HashMap<String, ToolOutcome> =
            std::collections::HashMap::new();
        loop {
            let active = fallback.clone().unwrap_or_else(|| route.choice.clone());
            let (provider, model) = self.resolve_provider(&active)?;

            self.store
                .append(
                    session_id,
                    &SessionEvent::ModelSelected {
                        model: model.clone(),
                        reason: active.reason.clone(),
                        action: Some(format!("{:?}", route.action)),
                        context_tokens: Some(route.context_tokens),
                    },
                )
                .await?;

            // Shared by Arc: retries/escalation re-send it without
            // re-cloning the transcript.
            let request = std::sync::Arc::new(TurnRequest {
                model: model.clone(),
                system: system.to_string(),
                messages: state.transcript.clone(),
                tools: tool_specs.to_vec(),
                max_tokens: MAX_OUTPUT_TOKENS,
            });
            tracing::info!(
                turn = state.turns + 1,
                provider = provider.id(),
                model = %model,
                "turn start"
            );
            // Streaming dispatch (Phase-1c I1): when a delta sink is
            // configured, drive the provider through `turn_streaming` and
            // forward deltas as they arrive (providers without an SSE
            // implementation yield one Text delta via the trait default).
            turn_started = std::time::Instant::now();
            speculative.clear();
            let response = match self.config.on_delta.clone() {
                Some(sink) => {
                    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<StreamDelta>();
                    // Speculative driver: forwards deltas to the sink,
                    // and when the provider completes a tool-call item
                    // mid-stream, immediately executes idempotent reads
                    // so the dispatch below can reuse the result instead
                    // of re-running the tool. Gated OFF when any observer
                    // or gate would be bypassed: pre_tool/post_tool
                    // hooks, the on_tool_event sink, and the --deny list
                    // all must keep seeing every execution
                    // ([`speculative::allowed_for`]).
                    if speculative::allowed_for(&self.config) {
                        let driver = speculative::SpeculativeDriver::spawn(
                            Arc::new(self.clone()),
                            session_id,
                            sink,
                            rx,
                        );
                        let result = provider.turn_streaming(request, tx).await;
                        speculative.extend(driver.shutdown().await);
                        result
                    } else {
                        // Plain forwarder: speculation disabled (hooks,
                        // event sink, or deny list present).
                        let forwarder = speculative::spawn_forwarder(sink, rx);
                        let result = provider.turn_streaming(request, tx).await;
                        let _ = forwarder.await;
                        result
                    }
                }
                None => provider.turn(request).await,
            };
            match response {
                Ok(r) => {
                    turn_outcome = Some((r, model, provider.id().to_string()));
                    break;
                }
                Err(e) => {
                    // The self-check turn is an ENHANCEMENT: if the
                    // provider fails on the turn right after the self-check
                    // message, keep the pre-check answer and end the run —
                    // never destroy completed work for the bonus turn. A
                    // failure on any later turn is a real error (A6).
                    if self_check_turn && !state.answer.is_empty() {
                        tracing::warn!(
                            error = %e,
                            "self-check turn failed; keeping the pre-check answer"
                        );
                        break;
                    }
                    // Auth-class failure on a tier's primary: retry ONCE
                    // on the tier's fallback candidate (a dead token on
                    // one provider must not take a tier another signed-in
                    // provider serves). Auth failures precede streaming,
                    // so nothing is re-emitted.
                    if !attempted_fallback {
                        if let Some(fb) =
                            self.tier_fallback_after_auth_failure(&model, provider.id(), &e)
                        {
                            tracing::warn!(
                                from = %model,
                                to = %fb.model,
                                to_provider = %fb.provider,
                                error = %e,
                                "auth failure — falling back to the tier fallback model"
                            );
                            attempted_fallback = true;
                            fallback = Some(fb.clone());
                            state.run_tier_override = Some((model, fb));
                            continue;
                        }
                    }
                    // Phase 13 completion (2026-11-11): a FAILED run
                    // is a failure signal for the memories it surfaced
                    // — the ledger learns from both outcomes, not just
                    // successes.
                    if !state.memory_hits.is_empty() {
                        if let Some(root) = &self.config.memory_root {
                            if let Ok(store) = crate::memory::MemoryStore::for_workspace(
                                root,
                                &self.config.working_dir,
                            ) {
                                store.record_outcome(&state.memory_hits, false);
                            }
                        }
                    }
                    return Err(e);
                }
            }
        }
        let Some((response, model, provider_id)) = turn_outcome else {
            // Self-check short-circuit: the outer run loop ends with
            // the preserved answer.
            return Ok(None);
        };
        Ok(Some(CompletedTurn {
            response,
            model,
            provider_id,
            started: turn_started,
            speculative,
        }))
    }

    /// Dispatch phase: arm the finish gate when a mutating call was
    /// requested, execute every tool call through the hook pipeline
    /// (concurrently when all read-only), and bookkeep each result —
    /// delegated spend, `ToolResult` events, transcript tool messages,
    /// memory hits, verify evidence.
    async fn dispatch_turn_tools(
        &self,
        session_id: SessionId,
        tool_calls: &[ToolCall],
        model: &str,
        speculative: &std::collections::HashMap<String, ToolOutcome>,
        state: &mut RunLoopState,
    ) -> Result<()> {
        // Dispatch tool calls; each result enters the transcript. Hooks
        // (H1) wrap execution: pre_tool may veto before the tool runs,
        // post_tool observes after. Hook-spawn errors fail closed.
        // Checkpoints (C1, A3) capture the prior content of files that
        // write/edit are about to mutate; they are recorded inside
        // `execute_tool` after the approval gate allows the call, in every
        // approval mode, so a refused call records nothing.
        let ctx = ToolCtx {
            sandbox: self.config.sandbox,
            working_dir: self.config.working_dir.clone(),
            engine: Some(Arc::new(self.clone())),
            session_id: Some(session_id),
        };
        // Phase 4: when every call in the turn is read-only, they run
        // concurrently (order-preserving — the transcript is identical to
        // the sequential case, only faster). Mutating calls stay
        // sequential: checkpoints and approval gating are order-sensitive.
        // Gated calls (A1) also stay sequential: approval prompts must not
        // interleave.
        // A gated call only forces the sequential path when it would
        // actually prompt; under ApproveAll or an allow-list entry it keeps
        // the concurrency it had before the gate widened (parallel
        // delegates).
        let prompts = |name: &str| {
            is_gated(name)
                && self.config.approvals.mode != ApprovalMode::ApproveAll
                && !self.config.approvals.allow.iter().any(|a| a == name)
        };
        let all_read_only = tool_calls
            .iter()
            .all(|c| !FILE_MUTATING_TOOLS.contains(&c.name.as_str()) && !prompts(&c.name));
        // A speculative result was computed mid-stream, before this turn's
        // calls ran. It is only valid for a call when no file-mutating call
        // precedes it in this turn; otherwise `[edit X, read X]` would reuse
        // the pre-edit content of X.
        let mut mutated_earlier_in_turn = false;
        let speculation_ok: Vec<bool> = tool_calls
            .iter()
            .map(|call| {
                let ok = !mutated_earlier_in_turn;
                if FILE_MUTATING_TOOLS.contains(&call.name.as_str()) {
                    mutated_earlier_in_turn = true;
                }
                ok
            })
            .collect();
        let dispatched: Vec<(ToolOutcome, bool)> = if all_read_only && tool_calls.len() > 1 {
            let futures = tool_calls.iter().zip(&speculation_ok).map(|(call, ok)| {
                self.dispatch_tool_call(session_id, call, &ctx, speculative, *ok)
            });
            futures::future::join_all(futures).await
        } else {
            let mut dispatched = Vec::with_capacity(tool_calls.len());
            for (call, ok) in tool_calls.iter().zip(&speculation_ok) {
                let result = self
                    .dispatch_tool_call(session_id, call, &ctx, speculative, *ok)
                    .await;
                dispatched.push(result);
            }
            dispatched
        };
        // A mutating call arms the finish gate only once it actually ran
        // (A5): a call refused by deny/approval, vetoed by a hook, or
        // stopped at the checkpoint never touched the workspace, so it
        // must not make the run owe verification.
        if tool_calls
            .iter()
            .zip(&dispatched)
            .any(|(c, (_, ran))| *ran && FILE_MUTATING_TOOLS.contains(&c.name.as_str()))
        {
            state.mutated_this_run = true;
        }
        let outcomes: Vec<ToolOutcome> =
            dispatched.into_iter().map(|(outcome, _)| outcome).collect();
        state.last_dispatch_ended_with_passing_verify = tool_calls
            .last()
            .map(|c| c.name == "verify")
            .unwrap_or(false)
            && outcomes.last().map(|o| !o.is_error).unwrap_or(false);

        for (call, outcome) in tool_calls.iter().zip(&outcomes) {
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
                state
                    .memory_hits
                    .extend(ids.iter().filter_map(|i| i.as_str().map(String::from)));
            }
            // Delegated spend (Phase 5c A2): usage spent behind a tool's
            // interface joins the governor and the run totals — honest
            // accounting to the token.
            if let Some(usage) = outcome.usage {
                state.governor.feed_turn(usage);
                state.spend_usd += self.turn_cost_usd(model, &usage);
                add_usage(&mut state.total, &usage);
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
            state
                .transcript
                .push(Message::tool_result(call.id.clone(), outcome));
            state.tool_calls_executed += 1;
        }
        Ok(())
    }

    /// Finalize phase (post-loop): the success signal for the memories this
    /// run surfaced, the terminal trace record with the stop label, and the
    /// run summary.
    async fn finalize_run(
        &self,
        session_id: SessionId,
        state: &mut RunLoopState,
    ) -> Result<RunSummary> {
        // Phase 13 self-improving memory: a completed run is a success
        // signal for every memory it surfaced. (Failures currently go
        // unrecorded — an errored run returns Err before this point.)
        if !state.memory_hits.is_empty() {
            if let Some(root) = &self.config.memory_root {
                if let Ok(store) =
                    crate::memory::MemoryStore::for_workspace(root, &self.config.working_dir)
                {
                    store.record_outcome(&state.memory_hits, true);
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
                    turn: state.turns,
                    harness_version: env!("CARGO_PKG_VERSION").to_string(),
                    provider: String::new(),
                    model: String::new(),
                    prompt_hash: None,
                    latency_ms: None,
                    cost_usd: None,
                    usage: None,
                    verifier: None,
                    failure_label: Some(state.stop_label.to_string()),
                },
            )
            .await;
        Ok(RunSummary {
            session_id,
            answer: std::mem::take(&mut state.answer),
            turns: state.turns,
            tool_calls: state.tool_calls_executed,
            usage: std::mem::take(&mut state.total),
            models_used: std::mem::take(&mut state.models_used),
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
    /// Auth-class provider failure: the request never reached the model
    /// (dead token, revoked grant, bad key), so retrying on another
    /// provider cannot duplicate streamed output. Message-shape based —
    /// providers format status + provider error bodies into `message`.
    fn is_auth_failure(error: &Error) -> bool {
        match error {
            Error::Provider { message, .. } => {
                if has_http_status_prefix(message, "401") || has_http_status_prefix(message, "403")
                {
                    return true;
                }
                let m = message.to_ascii_lowercase();
                [
                    "unauthorized",
                    "revoked",
                    "invalid_grant",
                    "authentication_error",
                    "invalid api key",
                    "invalid_api_key",
                ]
                .iter()
                .any(|needle| m.contains(needle))
            }
            _ => false,
        }
    }

    /// The first same-tier fallback candidate on a different, registered
    /// provider — only when `failed_model` is a tier's primary and the
    /// error is auth-class.
    fn tier_fallback_after_auth_failure(
        &self,
        failed_model: &str,
        failed_provider: &str,
        error: &Error,
    ) -> Option<ModelChoice> {
        if !Self::is_auth_failure(error) {
            return None;
        }
        let fallbacks = self.config.tier_fallbacks.as_ref()?;
        let tier_list = if fallbacks
            .cheap
            .first()
            .is_some_and(|(m, _)| m == failed_model)
        {
            ("cheap", &fallbacks.cheap)
        } else if fallbacks
            .strong
            .first()
            .is_some_and(|(m, _)| m == failed_model)
        {
            ("strong", &fallbacks.strong)
        } else {
            return None;
        };
        tier_list
            .1
            .iter()
            .skip(1)
            .find(|(_, provider)| {
                provider != failed_provider && self.providers.contains_key(provider)
            })
            .map(|(model, provider)| ModelChoice {
                model: model.clone(),
                provider: provider.clone(),
                reason: format!(
                    "tier fallback ({}): {failed_model} failed auth on {failed_provider}",
                    tier_list.0
                ),
                confident: true,
                context_window: None,
            })
    }

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
    /// instead of the run dying. Gated tools ([`is_gated`]) pass the
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
        if !matches!(call.name.as_str(), "write" | "edit") {
            return Ok(None);
        }
        let Some(path) = call.arguments.get("path").and_then(|v| v.as_str()) else {
            return Ok(None);
        };
        // Only paths that resolve inside the working dir are checkpointed
        // (A3): anything else records nothing.
        let Some(resolved) = resolve_inside(&self.config.working_dir, path) else {
            tracing::warn!(path = %path, "checkpoint skipped: path resolves outside the working dir");
            return Ok(None);
        };
        let (prior_content_b64, uncaptured) = match prior_state(&resolved) {
            PriorState::Absent => (None, false),
            PriorState::Captured(encoded) => (Some(encoded), false),
            PriorState::Uncaptured => (None, true),
        };
        // event_count, not load: this runs per mutating tool call, and a
        // full log parse per call compounds over a write-heavy run (2026-11-12
        // audit perf fix).
        let event_index = self.store.event_count(session_id).await? + 1;
        Ok(Some(SessionEvent::Checkpoint {
            path: path.to_string(),
            prior_content_b64,
            event_index,
            uncaptured,
        }))
    }

    /// Record the pre-mutation checkpoint for an allowed call (A3). Called
    /// from `execute_tool` only after the deny and approval gates pass, so a
    /// refused call records nothing.
    async fn record_checkpoint(&self, session_id: SessionId, call: &ToolCall) -> Result<()> {
        if let Some(checkpoint) = self.checkpoint_for(session_id, call).await? {
            self.store.append(session_id, &checkpoint).await?;
        }
        Ok(())
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
    ) -> Result<(ToolOutcome, bool)> {
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
                return Ok((
                    ToolOutcome::err(format!("blocked by pre_tool hook: {}", outcome.message)),
                    false,
                ));
            }
        }

        if let Some(sink) = self.config.on_tool_event.as_ref() {
            sink(ToolActivity::Start {
                name: call.name.clone(),
            });
        }
        let (executed, ran) = self.execute_tool(session_id, call, ctx).await;
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
        Ok((executed, ran))
    }

    /// The approval policy's verdict for a tool name (A1, A5): the deny list
    /// first (any mode), then the ungated / allow-listed / ApproveAll fast
    /// path, then Ask-with-approver (a prompt), otherwise refused. Pure rules:
    /// model-issued calls and the finish gate both route through here.
    fn admission(&self, name: &str) -> Admission {
        if self.config.approvals.deny.iter().any(|d| d == name) {
            return Admission::Refused(format!("tool `{name}` denied by configuration (--deny)"));
        }
        let explicitly_allowed = self.config.approvals.allow.iter().any(|a| a == name);
        if !is_gated(name)
            || explicitly_allowed
            || self.config.approvals.mode == ApprovalMode::ApproveAll
        {
            return Admission::Run;
        }
        if self.config.approvals.mode == ApprovalMode::Ask && self.config.approver.is_some() {
            return Admission::Ask;
        }
        let mode = self.config.approvals.mode;
        tracing::warn!(
            tool = %name,
            mode = ?mode,
            "tool blocked by approval mode — run with --approve-all to allow mutating tools"
        );
        Admission::Refused(approval_block_hint(mode))
    }

    /// Whether the finish gate may run `verify` right now (A5). Uses the
    /// same admission rules as a model-issued call: an Ask decision goes to
    /// the approver, and a refusal or a missing approver skips verification.
    fn finish_gate_may_verify(&self) -> bool {
        match self.admission("verify") {
            Admission::Run => true,
            Admission::Ask => {
                let call = ToolCall {
                    id: "finish-gate-verify".into(),
                    name: "verify".into(),
                    arguments: serde_json::json!({}),
                };
                self.config
                    .approver
                    .as_ref()
                    .is_some_and(|approver| approver(&call))
            }
            Admission::Refused(_) => false,
        }
    }

    /// Execute one call through the approval gate and the pre-mutation
    /// checkpoint. The flag is `true` only when the tool itself ran (A5), so
    /// a refused, unknown or checkpoint-blocked call never arms the finish gate.
    async fn execute_tool(
        &self,
        session_id: SessionId,
        call: &ToolCall,
        ctx: &ToolCtx,
    ) -> (ToolOutcome, bool) {
        let Some(tool) = self.tools.get(&call.name).cloned() else {
            let available = self
                .tools
                .specs()
                .iter()
                .map(|spec| spec.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return (
                ToolOutcome::err(format!(
                    "unknown tool `{}` — available tools: {available}",
                    call.name
                )),
                false,
            );
        };

        match self.admission(&call.name) {
            Admission::Run => {}
            Admission::Ask => {
                // Ask mode with an interactive approver (Phase-1c I2): a
                // denial falls through to the block.
                let approved = self
                    .config
                    .approver
                    .as_ref()
                    .is_some_and(|approver| approver(call));
                if !approved {
                    return (
                        ToolOutcome::err(approval_block_hint(self.config.approvals.mode)),
                        false,
                    );
                }
            }
            Admission::Refused(message) => return (ToolOutcome::err(message), false),
        }

        // Pre-mutation checkpoint (D8, A3): taken only now, after the gates
        // allowed the call, in every approval mode. If it cannot be recorded
        // the call fails closed: a write rewind cannot undo must not run.
        if let Err(err) = self.record_checkpoint(session_id, call).await {
            return (
                ToolOutcome::err(format!(
                    "checkpoint not recorded; `{}` did not run: {err}",
                    call.name
                )),
                false,
            );
        }

        // From here the tool runs, whatever it returns: a bash call that
        // wrote before failing has still mutated the workspace.
        let outcome = match tool.execute(call.arguments.clone(), ctx).await {
            Ok(outcome) => outcome,
            Err(err) => ToolOutcome::err(format!("tool `{}` failed to run: {err}", call.name)),
        };
        (outcome, true)
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
        state: &mut RunLoopState,
    ) -> Result<bool> {
        if !state.mutated_this_run {
            return Ok(false);
        }
        let Some(verify_tool) = self.tools.get("verify") else {
            // No verify tool: the self-check fallback (when enabled) forces
            // ONE requirements-checklist turn — the model re-reads the task
            // and verifies each concrete detail against the environment
            // before ending. Targets the lucky-pass class (config formats,
            // exact names/passwords/ports) without needing a test command.
            // At most one self-check per run (A6). No verification runs on
            // this path, so the gate disarms here: nothing can "still fail",
            // and the failure note and escalation (both verify-only) stay off.
            if !self.config.self_check_on_finish || state.self_check_sent {
                return Ok(false);
            }
            state.self_check_sent = true;
            state.self_check_turn_pending = true;
            state.mutated_this_run = false;
            let feedback = Message::user(
                "Before finishing: re-read the ORIGINAL task instruction and list every \
                 concrete requirement it names — file paths, exact names, passwords, \
                 ports, config file locations and formats, expected content. Verify each \
                 one against the actual environment with tools (read the configs you \
                 wrote, check services are still running, confirm file contents). Fix \
                 any mismatch you find, then finish.",
            );
            self.store
                .append(
                    session_id,
                    &SessionEvent::Message {
                        parent_id: None,
                        message: feedback.clone(),
                    },
                )
                .await?;
            state.transcript.push(feedback);
            return Ok(true);
        };
        if state.verify_rounds >= MAX_VERIFY_ROUNDS {
            // Bound exhausted: let the run end, but never silently.
            state.answer = format!(
                "{}\n\n[verification still failing after {MAX_VERIFY_ROUNDS} rounds — see transcript]",
                state.answer
            )
            .trim()
            .to_string();
            return Ok(false);
        }
        // Skip when the previous dispatch already ended with a passing
        // verify (the model showed its evidence — no duplicate suite run).
        if state.last_dispatch_ended_with_passing_verify {
            state.mutated_this_run = false;
            return Ok(false);
        }
        // A5: the harness runs verify only where the approval policy would
        // let the model run it (same admission rules as a model-issued call).
        // Otherwise the gate stands down, the skip is stated, and the run ends.
        if !self.finish_gate_may_verify() {
            tracing::warn!("finish gate skipped verification: verify is not permitted");
            state.mutated_this_run = false;
            state.answer = format!(
                "{}\n\n[verification skipped: verify is not permitted in this approval mode]",
                state.answer
            )
            .trim()
            .to_string();
            return Ok(false);
        }
        state.verify_rounds += 1;
        let ctx = ToolCtx {
            sandbox: self.config.sandbox,
            working_dir: self.config.working_dir.clone(),
            engine: Some(Arc::new(self.clone())),
            session_id: None,
        };
        let outcome = verify_tool
            .execute(serde_json::json!({}), &ctx)
            .await
            .unwrap_or_else(|err| ToolOutcome::err(err.to_string()));
        tracing::info!(
            round = state.verify_rounds,
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
        state.transcript.push(feedback);
        if passed {
            state.mutated_this_run = false;
            return Ok(false);
        }
        Ok(true)
    }

    /// ONE best-effort escalation turn after the finish gate exhausted its
    /// feedback rounds (`escalate_on_check_failure` set): the configured
    /// model re-checks every concrete requirement and its answer REPLACES
    /// the run answer on success. Every failure mode degrades to "keep the
    /// pre-escalation answer" (warned, `Ok` returned) — the gate's give-up
    /// note in `answer` is already an honest end state, so a failed
    /// escalation must never destroy it, let alone fail the run.
    ///
    /// Shape mirrors [`Self::run_compaction`]: a harness-initiated model
    /// call outside the main loop (plain `provider.turn`, full usage
    /// accounting by the caller's locals). Tool-less by design: a single
    /// turn cannot service tool calls it provokes, and a persisted
    /// assistant message with unanswered tool_calls would break provider
    /// request building on resume (the same orphan-record concern that
    /// keeps the gate's feedback a user message).
    ///
    /// Store errors propagate (infrastructure — the run is already broken);
    /// provider/resolution failures are swallowed here.
    async fn run_escalation_turn(
        &self,
        session_id: SessionId,
        task: &str,
        system: &str,
        state: &mut RunLoopState,
    ) -> Result<()> {
        let escalation_model = self
            .config
            .escalate_on_check_failure
            .clone()
            .ok_or_else(|| {
                Error::Config(
                    "escalation turn requested without `escalate_on_check_failure`".into(),
                    "set `escalate_on_check_failure` in the engine config".into(),
                )
            })?;

        // Provider resolution mirrors the persona path (subagent.rs
        // ForcedSelector → resolve_provider): keep the selector's provider
        // for a Chat action and swap only the model, so the registered-
        // provider → default-provider fallback ladder applies unchanged. A
        // model id from another provider rides the run's provider — a
        // mismatch surfaces as a provider error and degrades below.
        let context_tokens = estimate_context_tokens(system, &[], &state.transcript);
        let mut choice = self.selector.select(&SelectRequest {
            task: task.to_string(),
            action: ActionKind::Chat,
            context_tokens,
            budget_remaining_usd: self
                .config
                .max_cost_usd
                .map(|cap| (cap - state.spend_usd).max(0.0)),
        });
        choice.model = escalation_model.clone();
        choice.reason = format!(
            "escalation: verification failed after {MAX_VERIFY_ROUNDS} rounds → {escalation_model}"
        );
        choice.confident = true;
        let (provider, model) = self.resolve_provider(&choice)?;
        // resolve_provider's fallback swaps the MODEL when the selector's
        // provider is unregistered — an escalation that silently runs on
        // default_model would lie about "the escalation model re-checked
        // this". Abort the escalation instead; the old answer survives.
        if model != escalation_model {
            tracing::warn!(
                requested = %escalation_model,
                resolved = %model,
                "escalation model did not resolve; keeping the pre-escalation answer"
            );
            return Ok(());
        }

        self.store
            .append(
                session_id,
                &SessionEvent::ModelSelected {
                    model: model.clone(),
                    reason: choice.reason.clone(),
                    action: Some(format!("{:?}", ActionKind::Chat)),
                    context_tokens: Some(context_tokens),
                },
            )
            .await?;

        // Handoff to the escalated model, persisted as a user message (the
        // gate's feedback pattern): resume and compaction rebuild the
        // transcript from the event log, so the escalation's context must
        // live in a `Message` event, not in a transient.
        let handoff = Message::user(format!(
            "Verification failed after {MAX_VERIFY_ROUNDS} feedback rounds. This run is \
             escalated to `{model}` for one re-check: re-read the ORIGINAL task and verify \
             every concrete requirement against the evidence above, then give the corrected \
             final answer."
        ));
        self.store
            .append(
                session_id,
                &SessionEvent::Message {
                    parent_id: None,
                    message: handoff.clone(),
                },
            )
            .await?;
        state.transcript.push(handoff);

        let request = std::sync::Arc::new(TurnRequest {
            model: model.clone(),
            system: format!("{system}{ESCALATION_SYSTEM_ADDENDUM}"),
            messages: state.transcript.clone(),
            // No tools: see the doc comment — a tool call the harness
            // cannot service must not be provoked.
            tools: Vec::new(),
            max_tokens: MAX_OUTPUT_TOKENS,
        });
        tracing::info!(
            provider = provider.id(),
            model = %model,
            "escalation turn: re-checking after verification failure"
        );
        let turn_started = std::time::Instant::now();
        // Best-effort by contract: a provider failure keeps the
        // pre-escalation answer (which already carries the gate's give-up
        // note) and ends the run — never a failed run for a bonus turn.
        let response = match provider.turn(request).await {
            Ok(response) => response,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "escalation turn failed; keeping the pre-escalation answer"
                );
                return Ok(());
            }
        };

        // Honest accounting to the token: the escalation's usage joins the
        // governor, the run totals, the event log and the USD spend exactly
        // like a main-loop turn (compaction precedent).
        state.turns += 1;
        state.governor.feed_turn(response.usage);
        add_usage(&mut state.total, &response.usage);
        self.store
            .append(
                session_id,
                &SessionEvent::Usage {
                    model: model.clone(),
                    usage: response.usage,
                },
            )
            .await?;
        if !state.models_used.contains(&model) {
            state.models_used.push(model.clone());
        }
        let turn_cost = self.turn_cost_usd(&model, &response.usage);
        state.spend_usd += turn_cost;
        self.store
            .append(
                session_id,
                &SessionEvent::TurnTrace {
                    turn: state.turns,
                    harness_version: env!("CARGO_PKG_VERSION").to_string(),
                    provider: provider.id().to_string(),
                    model: model.clone(),
                    // No prompt hash: the one-off system addendum breaks the
                    // stable-prefix contract the hash audits — recording a
                    // divergent hash would read as a cache bug, not the
                    // exceptional turn it is.
                    prompt_hash: None,
                    latency_ms: Some(turn_started.elapsed().as_millis() as u64),
                    cost_usd: Some(turn_cost),
                    usage: Some(response.usage),
                    verifier: None,
                    failure_label: None,
                },
            )
            .await?;

        // Record the escalated answer as the assistant message it stands
        // for (transcript stays rebuildable from the log).
        let assistant = Message {
            role: Role::Assistant,
            content: response.text.clone(),
            tool_calls: Vec::new(),
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
        state.transcript.push(assistant);

        // An empty escalation is no replacement: models that refuse or
        // truncate yield "" and the honest give-up note must survive it.
        if response.text.trim().is_empty() {
            tracing::warn!("escalation turn returned no text; keeping the pre-escalation answer");
            return Ok(());
        }
        state.answer = response.text;
        Ok(())
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
        speculation_ok: bool,
    ) -> (ToolOutcome, bool) {
        // `speculation_ok` is false when a file-mutating call precedes this
        // one in the same turn: the mid-stream result may be stale.
        if speculation_ok
            && speculative::SPECULATIVE_TOOLS.contains(&call.name.as_str())
            && !self.config.approvals.deny.iter().any(|d| d == &call.name)
        {
            if let Some(outcome) = speculative.get(&speculative::speculative_key(call)) {
                // The empty error is the driver's placeholder: a task that
                // panicked before overwriting it must NOT feed the model an
                // empty tool result - fall through and execute for real
                // (cycle-2 audit M6).
                if !(outcome.is_error && outcome.content.is_empty()) {
                    tracing::info!(tool = %call.name, "speculative hit: reuse mid-stream result");
                    // Speculation only covers read-only tools, so nothing
                    // mutated here.
                    return (outcome.clone(), false);
                }
                tracing::warn!(tool = %call.name, "speculative placeholder left by a panicked task - re-executing");
            }
        }
        self.execute_tool_with_hooks(session_id, call, ctx)
            .await
            .unwrap_or_else(|err| (ToolOutcome::err(err.to_string()), false))
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
        // Same view as a resume: a rewound range must not be summarized
        // back into context by a later compaction.
        let events = rewind_view(
            &events,
            self.config.resume_from_event,
            self.config.resume_skip_end,
            &self.config.resume_skip_ranges,
        )
        .into_owned();
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
        // Only what the previous compaction did not already cover (A7),
        // capped so the summarizer request cannot outgrow its window.
        let older_text =
            compact::incremental_transcript_text(&events, self.config.compaction.tail_messages);

        // Route the summarization (cheap model for Summarize per PLAN §4.3).
        let choice = self.selector.select(&SelectRequest {
            task: task.to_string(),
            action: ActionKind::Summarize,
            context_tokens: estimate_tokens(&older_text),
            budget_remaining_usd: None,
        });
        let (provider, model) = self.resolve_provider(&choice)?;

        let request = std::sync::Arc::new(TurnRequest {
            model: model.clone(),
            system: compact::summarizer_prompt().to_string(),
            messages: vec![Message::user(older_text.as_str())],
            tools: Vec::new(),
            max_tokens: COMPACTION_MAX_OUTPUT_TOKENS,
        });
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

/// Rewrite a rewind-cut slice so every kept assistant `tool_use` has its
/// `tool_result`. The cut lands before abandoned results, but a MULTI-call
/// turn interleaves them (`[assistant(c1,c2), ToolResult(c1),
/// Checkpoint(c2), ToolResult(c2)]`): a tail-only trim still leaves c2
/// unanswered, and both provider APIs reject tool_use without tool_result
/// (cycle-2 audit H1; the cycle-3 adversarial pass caught the multi-call
/// gap). Tool calls without a result in the slice are dropped from their
/// assistant message; a message left with no calls AND no text disappears.
fn prune_unanswered_tool_calls(events: &[SessionEvent]) -> std::borrow::Cow<'_, [SessionEvent]> {
    let answered: std::collections::HashSet<&str> = events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::ToolResult { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    // Calls actually present in the slice; a result whose call is missing
    // is an orphan and would 400 the provider just like an unanswered call.
    let called: std::collections::HashSet<&str> = events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::Message { message, .. } if message.role == Role::Assistant => {
                Some(message.tool_calls.iter().map(|c| c.id.as_str()))
            }
            _ => None,
        })
        .flatten()
        .collect();
    let needs_rewrite = events.iter().any(|e| match e {
        SessionEvent::Message { message, .. } => {
            message.role == Role::Assistant
                && message
                    .tool_calls
                    .iter()
                    .any(|c| !answered.contains(c.id.as_str()))
        }
        SessionEvent::ToolResult { call_id, .. } => !called.contains(call_id.as_str()),
        _ => false,
    });
    if !needs_rewrite {
        return std::borrow::Cow::Borrowed(events);
    }
    let mut out = Vec::with_capacity(events.len());
    for event in events {
        match event {
            SessionEvent::Message { parent_id, message }
                if message.role == Role::Assistant
                    && message
                        .tool_calls
                        .iter()
                        .any(|c| !answered.contains(c.id.as_str())) =>
            {
                let mut kept = message.clone();
                kept.tool_calls.retain(|c| answered.contains(c.id.as_str()));
                if !kept.tool_calls.is_empty() || !kept.content.is_empty() {
                    out.push(SessionEvent::Message {
                        parent_id: *parent_id,
                        message: kept,
                    });
                }
            }
            // Checkpoints around the cut carry no context.
            SessionEvent::Checkpoint { .. } => {}
            // A result whose call is gone would be an orphan tool message.
            SessionEvent::ToolResult { call_id, .. } if !called.contains(call_id.as_str()) => {}
            other => out.push(other.clone()),
        }
    }
    std::borrow::Cow::Owned(out)
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
            // The plan is not a conversation message: the context builder
            // re-injects the latest plan every turn (the stays-on-track
            // mechanism), so raw plan writes stay out of the transcript.
            SessionEvent::Plan { .. } => {}
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
    let mut messages = match last_compaction {
        None => events_to_messages(events),
        Some((idx, summary)) => {
            let mut messages = vec![compact_state_message(summary)];
            let (_, tail) = compact::split_older_tail(&events[..idx], tail_messages);
            messages.extend(events_to_messages(tail));
            messages.extend(events_to_messages(&events[idx + 1..]));
            messages
        }
    };
    // The standing plan rides at the end of every turn (the
    // stays-on-track mechanism): the model re-reads its own commitments
    // before responding. One message; no plan writes to the transcript.
    if let Some(items) = latest_plan(events) {
        if !items.is_empty() {
            let mut body =
                String::from("[standing plan - keep statuses current via the plan tool]\n");
            for (ix, item) in items.iter().enumerate() {
                body.push_str(&format!(
                    "{}. [{}] {} - {}\n",
                    ix + 1,
                    item.status,
                    item.text,
                    item.done_criteria
                ));
            }
            messages.push(Message::user(body));
        }
    }
    messages
}

/// The most recent plan in the log, if any.
fn latest_plan(events: &[SessionEvent]) -> Option<Vec<modron_types::PlanItem>> {
    events.iter().rev().find_map(|event| match event {
        SessionEvent::Plan { items } => Some(items.clone()),
        _ => None,
    })
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
        // Attachments ride the first user message as base64 blocks; the
        // pre-flight budget check must see them or a big image spend goes
        // unaccounted until the provider rejects the request (cycle-2 L5).
        for attachment in &message.attachments {
            match attachment {
                crate::Attachment::Image { data, .. } => {
                    tokens += (data.len() as u64).div_ceil(4);
                }
                crate::Attachment::Document { text, .. } => {
                    tokens += estimate_tokens(text);
                }
            }
        }
        for call in &message.tool_calls {
            // Serialized-size estimate WITHOUT building the JSON string:
            // the per-turn pass walks every historical call, and the
            // to_string allocation per call was the O(context) churn
            // (2026-11-12 audit).
            tokens += estimate_tokens(&call.name) + 48 + json_serialized_len(&call.arguments);
        }
    }
    tokens
}

/// Approximate serialized-JSON byte length by traversal (no string built).
fn json_serialized_len(value: &serde_json::Value) -> u64 {
    use serde_json::Value;
    match value {
        Value::Null => 4,
        Value::Bool(_) => 5,
        Value::Number(n) => n.to_string().len() as u64,
        Value::String(s) => s.len() as u64 + 2,
        Value::Array(items) => {
            items.iter().map(json_serialized_len).sum::<u64>() + items.len() as u64 + 2
        }
        Value::Object(map) => {
            map.iter()
                .map(|(k, v)| k.len() as u64 + 3 + json_serialized_len(v))
                .sum::<u64>()
                + 2
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn rewind_view_skips_only_the_abandoned_range() {
        use super::rewind_view;
        let ev = |t: &str| crate::SessionEvent::Task { text: t.into() };
        let events = vec![ev("a"), ev("b"), ev("c"), ev("d"), ev("e")];
        let texts = |v: &[crate::SessionEvent]| -> Vec<String> {
            v.iter()
                .map(|e| match e {
                    crate::SessionEvent::Task { text } => text.clone(),
                    _ => String::new(),
                })
                .collect()
        };
        // No cut: everything.
        assert_eq!(texts(&rewind_view(&events, None, Some(3), &[])).len(), 5);
        // Cut only: the old meaning, everything from the cut is dropped.
        assert_eq!(texts(&rewind_view(&events, Some(2), None, &[])), ["a", "b"]);
        // Cut and end: [2, 4) abandoned, later work kept.
        assert_eq!(
            texts(&rewind_view(&events, Some(2), Some(4), &[])),
            ["a", "b", "e"]
        );
        // Restart (cut 0) keeps only post-restart work.
        assert_eq!(
            texts(&rewind_view(&events, Some(0), Some(3), &[])),
            ["d", "e"]
        );
        // End at or past the log end, or below the cut, never panics.
        assert_eq!(
            texts(&rewind_view(&events, Some(2), Some(99), &[])),
            ["a", "b"]
        );
        assert_eq!(
            texts(&rewind_view(&events, Some(4), Some(1), &[])),
            ["a", "b", "c", "d", "e"]
        );
        assert_eq!(texts(&rewind_view(&events, Some(99), None, &[])).len(), 5);
        // Earlier rewinds stay abandoned when a later one is recorded.
        assert_eq!(
            texts(&rewind_view(&events, Some(3), Some(4), &[(0, 1)])),
            ["b", "c", "e"]
        );
        assert_eq!(
            texts(&rewind_view(&events, None, None, &[(1, 3)])),
            ["a", "d", "e"]
        );
    }

    use super::*;
    use crate::{EngineConfig, Tool, ToolOutcome, ToolRegistry, TurnResponse};
    use async_trait::async_trait;
    use serde_json::json;

    #[test]
    fn prune_unanswered_tool_calls_trims_the_orphaned_rewind_tail() {
        use crate::ToolOutcome;
        let mut assistant_with_call = Message::assistant("reading the file");
        assistant_with_call.tool_calls = vec![ToolCall {
            id: "c1".into(),
            name: "read".into(),
            arguments: serde_json::json!({"path": "a.rs"}),
        }];
        let events = vec![
            SessionEvent::Task { text: "t".into() },
            SessionEvent::Message {
                parent_id: None,
                message: assistant_with_call,
            },
            SessionEvent::Checkpoint {
                path: "f".into(),
                prior_content_b64: None,
                event_index: 3,
                uncaptured: false,
            },
            SessionEvent::ToolResult {
                call_id: "c1".into(),
                outcome: ToolOutcome::ok("contents"),
            },
        ];
        // The rewind cut is at the checkpoint index: the slice keeps
        // [task, assistant(c1), checkpoint]. c1 was abandoned: the call is
        // stripped, the assistant's prose survives, the checkpoint goes.
        let pruned = prune_unanswered_tool_calls(&events[..3]);
        assert_eq!(pruned.len(), 2, "checkpoint dropped, prose kept");
        let kept = match &pruned[1] {
            SessionEvent::Message { message, .. } => message,
            other => panic!("expected assistant, got {other:?}"),
        };
        assert!(kept.tool_calls.is_empty());
        assert_eq!(kept.content, "reading the file");

        // MULTI-call turn (cycle-3 adversarial F2): results interleave, the
        // cut lands before the LAST result, and the kept assistant must
        // lose only the unanswered call - not the whole turn.
        let mut multi = Message::assistant("working on it");
        multi.tool_calls = vec![
            ToolCall {
                id: "c1".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path": "a.rs"}),
            },
            ToolCall {
                id: "c2".into(),
                name: "write".into(),
                arguments: serde_json::json!({"path": "b.rs"}),
            },
        ];
        let multi_events = [
            SessionEvent::Task { text: "t".into() },
            SessionEvent::Message {
                parent_id: None,
                message: multi,
            },
            SessionEvent::ToolResult {
                call_id: "c1".into(),
                outcome: ToolOutcome::ok("a"),
            },
            SessionEvent::Checkpoint {
                path: "b.rs".into(),
                prior_content_b64: None,
                event_index: 4,
                uncaptured: false,
            },
            SessionEvent::ToolResult {
                call_id: "c2".into(),
                outcome: ToolOutcome::ok("b"),
            },
        ];
        // Cut before the abandoned c2 result: keep [task, assistant(-c2),
        // ToolResult(c1)].
        let pruned = prune_unanswered_tool_calls(&multi_events[..4]);
        assert_eq!(pruned.len(), 3, "checkpoint stripped, turn kept");
        let kept_calls = match &pruned[1] {
            SessionEvent::Message { message, .. } => &message.tool_calls,
            other => panic!("expected assistant, got {other:?}"),
        };
        assert_eq!(kept_calls.len(), 1, "c2 (unanswered) dropped");
        assert_eq!(kept_calls[0].id, "c1");

        // An answered run (full log) is untouched.
        assert_eq!(prune_unanswered_tool_calls(&events).len(), events.len());

        // Orphan result (its call is not in the slice): dropped, so no
        // tool message reaches the provider without an assistant call.
        let orphan = vec![
            SessionEvent::Task { text: "t".into() },
            SessionEvent::ToolResult {
                call_id: "gone".into(),
                outcome: ToolOutcome::ok("stray"),
            },
            SessionEvent::Message {
                parent_id: None,
                message: Message::assistant("next"),
            },
        ];
        let pruned = prune_unanswered_tool_calls(&orphan);
        assert_eq!(pruned.len(), 2, "orphan result removed");
        assert!(!pruned
            .iter()
            .any(|e| matches!(e, SessionEvent::ToolResult { .. })));

        // A text-only trailing assistant stays.
        let text_only = vec![
            SessionEvent::Task { text: "t".into() },
            SessionEvent::Message {
                parent_id: None,
                message: Message::assistant("done, no tools"),
            },
        ];
        assert_eq!(
            prune_unanswered_tool_calls(&text_only).len(),
            text_only.len()
        );
    }

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
            config: approve_all_config(),
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
        async fn turn(&self, _req: std::sync::Arc<TurnRequest>) -> Result<TurnResponse> {
            unreachable!("streaming path under test")
        }
        async fn turn_streaming(
            &self,
            _req: std::sync::Arc<TurnRequest>,
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

    /// Streams one glob `ToolCallReady` mid-stream (speculated), then ends
    /// its first turn with `final_calls`; turn 2+ finishes with text.
    struct StreamThenCalls {
        turns: Arc<std::sync::atomic::AtomicUsize>,
        final_calls: Vec<ToolCall>,
    }
    #[async_trait]
    impl ModelProvider for StreamThenCalls {
        fn id(&self) -> &str {
            "spec-stream"
        }
        async fn turn(&self, _req: std::sync::Arc<TurnRequest>) -> Result<TurnResponse> {
            unreachable!("streaming path under test")
        }
        async fn turn_streaming(
            &self,
            _req: std::sync::Arc<TurnRequest>,
            tx: tokio::sync::mpsc::UnboundedSender<StreamDelta>,
        ) -> Result<TurnResponse> {
            if self.turns.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0 {
                return Ok(text("done"));
            }
            let speculated = ToolCall {
                id: "spec-glob".into(),
                name: "glob".into(),
                arguments: json!({"pattern": "*.rs"}),
            };
            let _ = tx.send(StreamDelta::ToolCallReady(speculated));
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            Ok(TurnResponse {
                text: String::new(),
                tool_calls: self.final_calls.clone(),
                usage: Default::default(),
                stop_reason: StopReason::ToolUse,
            })
        }
    }

    /// Run one streamed turn whose final calls are `final_calls`; returns
    /// how many times glob actually executed.
    async fn glob_executions_for_turn(final_calls: Vec<ToolCall>) -> usize {
        use std::collections::HashMap;
        let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register(std::sync::Arc::new(CountingGlob(Arc::clone(&executions))));
        registry.register(std::sync::Arc::new(GateWriteTool));
        let mut providers: HashMap<String, Arc<dyn ModelProvider>> = HashMap::new();
        providers.insert(
            "spec-stream".into(),
            Arc::new(StreamThenCalls {
                turns: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                final_calls,
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
                    mode: ApprovalMode::ApproveAll,
                    ..crate::ApprovalPolicy::default()
                },
                ..EngineConfig::default()
            },
        };
        engine.run_task("edit then read").await.unwrap();
        executions.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[tokio::test]
    async fn speculative_read_after_a_mutation_in_the_same_turn_is_reexecuted() {
        // [edit X, read X]: the mid-stream glob ran before the write, so the
        // dispatch must run glob again rather than reuse the stale result.
        let executions = glob_executions_for_turn(vec![
            ToolCall {
                id: "w1".into(),
                name: "write".into(),
                arguments: json!({}),
            },
            ToolCall {
                id: "g1".into(),
                name: "glob".into(),
                arguments: json!({"pattern": "*.rs"}),
            },
        ])
        .await;
        assert_eq!(
            executions, 2,
            "speculated glob + a real re-execution after the write"
        );
    }

    #[tokio::test]
    async fn speculative_read_before_a_mutation_is_still_reused() {
        // [read X, edit X]: nothing mutated before the read, so the
        // speculated result is still valid and reused (one execution).
        let executions = glob_executions_for_turn(vec![
            ToolCall {
                id: "g1".into(),
                name: "glob".into(),
                arguments: json!({"pattern": "*.rs"}),
            },
            ToolCall {
                id: "w1".into(),
                name: "write".into(),
                arguments: json!({}),
            },
        ])
        .await;
        assert_eq!(
            executions, 1,
            "read before any mutation reuses the speculation"
        );
    }

    #[test]
    fn auth_failure_matches_the_http_status_prefix_only() {
        let provider_err = |message: &str| Error::Provider {
            provider: "p".into(),
            message: message.into(),
        };
        assert!(Engine::is_auth_failure(&provider_err(
            "HTTP 401 Unauthorized: bad key"
        )));
        assert!(Engine::is_auth_failure(&provider_err(
            "HTTP 403 Forbidden: no access"
        )));
        // A 429 whose body mentions "40300 tokens" is not an auth failure.
        assert!(!Engine::is_auth_failure(&provider_err(
            "HTTP 429 Too Many Requests: request asks for 40300 tokens"
        )));
        // A non-auth status whose body quotes 401 is not an auth failure.
        assert!(!Engine::is_auth_failure(&provider_err(
            "HTTP 500 Internal Server Error: upstream said 401"
        )));
    }

    #[tokio::test]
    async fn spend_cap_stop_keeps_the_capped_turn_text() {
        use std::collections::HashMap;
        let mut providers: HashMap<String, Arc<dyn ModelProvider>> = HashMap::new();
        providers.insert(
            "mock".into(),
            Arc::new(crate::testing::MockProvider::with_script(vec![
                TurnResponse {
                    text: "final words".into(),
                    tool_calls: Vec::new(),
                    usage: Usage {
                        input_tokens: 1_000_000,
                        output_tokens: 0,
                        cached_input_tokens: 0,
                    },
                    stop_reason: StopReason::EndTurn,
                },
            ])),
        );
        let engine = Engine {
            selector: Arc::new(crate::testing::StaticSelector::new("m", "mock")),
            providers,
            tools: Arc::new(ToolRegistry::new()),
            store: Arc::new(crate::testing::InMemoryStore::default()),
            config: EngineConfig {
                max_cost_usd: Some(0.5),
                // $1 per million input tokens: the single turn costs $1 > $0.50.
                price_lookup: Some(Arc::new(|_: &str| Some((1.0, 1.0)))),
                ..EngineConfig::default()
            },
        };
        let summary = engine.run_task("big task").await.unwrap();
        assert!(
            summary.answer.starts_with("final words"),
            "the capped turn's text is kept: {:?}",
            summary.answer
        );
        assert!(
            summary.answer.contains("spend cap"),
            "the cap note is still appended: {:?}",
            summary.answer
        );
    }

    #[tokio::test]
    async fn usage_totals_saturate_instead_of_wrapping() {
        // Hostile counts: turn 1 reports u64::MAX, turn 2 adds more. Plain
        // `+=` would wrap (release) or panic (debug) on the second add.
        use std::collections::HashMap;
        let huge = Usage {
            input_tokens: u64::MAX,
            output_tokens: u64::MAX,
            cached_input_tokens: u64::MAX,
        };
        let mut providers: HashMap<String, Arc<dyn ModelProvider>> = HashMap::new();
        providers.insert(
            "mock".into(),
            Arc::new(crate::testing::MockProvider::with_script(vec![
                TurnResponse {
                    usage: huge,
                    ..call("write")
                },
                TurnResponse {
                    usage: Usage {
                        input_tokens: 1,
                        output_tokens: 1,
                        cached_input_tokens: 1,
                    },
                    ..text("done")
                },
            ])),
        );
        let mut registry = ToolRegistry::new();
        registry.register(std::sync::Arc::new(GateWriteTool));
        let engine = Engine {
            selector: Arc::new(crate::testing::StaticSelector::new("m", "mock")),
            providers,
            tools: Arc::new(registry),
            store: Arc::new(crate::testing::InMemoryStore::default()),
            config: EngineConfig {
                max_total_tokens: u64::MAX,
                ..EngineConfig::default()
            },
        };
        let summary = engine.run_task("overflow me").await.unwrap();
        assert_eq!(summary.usage.input_tokens, u64::MAX);
        assert_eq!(summary.usage.output_tokens, u64::MAX);
        assert_eq!(summary.usage.cached_input_tokens, u64::MAX);
        assert_eq!(summary.turns, 2);
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
            config: approve_all_config(),
        };
        let summary = engine.run_task("ship it").await.unwrap();
        assert_eq!(
            summary.answer,
            "attempt 3

[verification still failing after 2 rounds — see transcript]"
        );
    }

    /// Gate engine variant for the escalation tests: AlwaysFail verify (so
    /// the bound IS exhausted) plus a handle on the mock to assert the
    /// escalation turn's wire shape.
    fn escalation_engine(
        script: Vec<TurnResponse>,
        escalate: Option<&str>,
    ) -> (Engine, std::sync::Arc<crate::testing::MockProvider>) {
        use std::collections::HashMap;
        let mut registry = ToolRegistry::new();
        registry.register(std::sync::Arc::new(GateWriteTool));
        registry.register(std::sync::Arc::new(AlwaysFailVerifyTool));
        let mock = std::sync::Arc::new(crate::testing::MockProvider::with_script(script));
        let mut providers: HashMap<String, std::sync::Arc<dyn ModelProvider>> = HashMap::new();
        providers.insert(
            "mock".into(),
            std::sync::Arc::clone(&mock) as std::sync::Arc<dyn ModelProvider>,
        );
        let engine = Engine {
            selector: std::sync::Arc::new(crate::testing::StaticSelector::new("m", "mock")),
            providers,
            tools: std::sync::Arc::new(registry),
            store: std::sync::Arc::new(crate::testing::InMemoryStore::default()),
            config: EngineConfig {
                escalate_on_check_failure: escalate.map(str::to_string),
                approvals: approve_all(),
                ..EngineConfig::default()
            },
        };
        (engine, mock)
    }

    #[tokio::test]
    async fn escalation_fires_once_when_the_gate_exhausts_its_rounds() {
        // Turns 1-4 drive the gate to its bound (write, then two failing
        // verify feedback rounds); the 5th scripted response serves the
        // escalation turn and must REPLACE the run answer.
        let (engine, mock) = escalation_engine(
            vec![
                call("write"),
                text("attempt 1"),
                text("attempt 2"),
                text("attempt 3"),
                text("ESCALATED ANSWER"),
            ],
            Some("claude-opus-escalated"),
        );
        let summary = engine.run_task("ship it").await.unwrap();
        assert_eq!(
            summary.answer, "ESCALATED ANSWER",
            "the escalated answer replaces the run answer (give-up note included)"
        );
        assert!(
            summary
                .models_used
                .contains(&"claude-opus-escalated".to_string()),
            "the escalation model is accounted in models_used: {:?}",
            summary.models_used
        );

        // Wire shape of the escalation turn: escalation model id, the
        // system addendum, no tools, and the persisted handoff message as
        // the last transcript entry (the gate's Message::user pattern).
        let requests = mock.requests();
        let escalated = requests.last().expect("escalation turn hit the provider");
        assert_eq!(
            escalated.model, "claude-opus-escalated",
            "the escalation turn must carry the escalation model id"
        );
        assert!(
            escalated.system.contains("VERIFICATION ESCALATION"),
            "the escalated turn carries the system-prompt addendum"
        );
        assert!(
            escalated.tools.is_empty(),
            "the escalation turn is tool-less: one turn cannot service tool calls"
        );
        assert!(
            matches!(escalated.messages.last(), Some(message) if message.role == Role::User
                && message.content.contains("escalated")),
            "the handoff rides the transcript as a user message"
        );

        // Exactly ONE extra turn: the four main-loop requests kept the
        // selector's model; the escalation was granted once.
        assert_eq!(requests.len(), 5);
        assert!(requests[..4].iter().all(|r| r.model == "m"));
        assert_eq!(summary.turns, 5);
    }

    #[tokio::test]
    async fn escalation_does_not_fire_when_unset() {
        // Same exhausted-gate script without `escalate_on_check_failure`:
        // the give-up note stays the final word and no fifth turn exists.
        let (engine, mock) = escalation_engine(
            vec![
                call("write"),
                text("attempt 1"),
                text("attempt 2"),
                text("attempt 3"),
            ],
            None,
        );
        let summary = engine.run_task("ship it").await.unwrap();
        assert!(summary
            .answer
            .contains("[verification still failing after 2 rounds"));
        // write + attempt 1 + attempt 2 + attempt 3 — no escalation turn.
        assert_eq!(mock.requests().len(), 4);
        assert!(mock.requests().iter().all(|r| r.model == "m"));
        assert_eq!(summary.turns, 4);
    }

    #[tokio::test]
    async fn escalation_provider_failure_keeps_the_pre_escalation_answer() {
        // The script covers only the main-loop turns, so the escalation
        // turn hits the mock's exhausted-script provider error. The run
        // must stay Ok, warn, and keep the honest give-up answer.
        let (engine, mock) = escalation_engine(
            vec![
                call("write"),
                text("attempt 1"),
                text("attempt 2"),
                text("attempt 3"),
            ],
            Some("claude-opus-escalated"),
        );
        let summary = engine.run_task("ship it").await.unwrap();
        assert_eq!(
            summary.answer,
            "attempt 3

[verification still failing after 2 rounds — see transcript]"
        );
        // The attempt reached the provider under the escalation model id
        // (recorded even though the turn errored) — once.
        let requests = mock.requests();
        assert_eq!(requests.len(), 5);
        assert_eq!(
            requests.last().map(|r| r.model.as_str()),
            Some("claude-opus-escalated")
        );
    }

    /// Mutating tools execute without prompting. The default policy is Ask
    /// with no approver, which blocks every edit, so finish-gate fixtures
    /// that expect a real edit must opt in.
    fn approve_all() -> crate::ApprovalPolicy {
        crate::ApprovalPolicy {
            mode: crate::ApprovalMode::ApproveAll,
            ..crate::ApprovalPolicy::default()
        }
    }

    fn approve_all_config() -> EngineConfig {
        EngineConfig {
            approvals: approve_all(),
            ..EngineConfig::default()
        }
    }

    /// Engine over the write tool (plus the counting verify tool when
    /// `with_verify`) with an explicit config. Returns the verify handle and
    /// the mock for request inspection.
    fn finish_engine(
        script: Vec<TurnResponse>,
        config: EngineConfig,
        with_verify: bool,
    ) -> (
        Engine,
        std::sync::Arc<GateVerifyTool>,
        std::sync::Arc<crate::testing::MockProvider>,
    ) {
        use std::collections::HashMap;
        let mut registry = ToolRegistry::new();
        registry.register(std::sync::Arc::new(GateWriteTool));
        let verify = std::sync::Arc::new(GateVerifyTool(std::sync::atomic::AtomicUsize::new(0)));
        if with_verify {
            registry.register(std::sync::Arc::clone(&verify) as std::sync::Arc<dyn Tool>);
        }
        let mock = std::sync::Arc::new(crate::testing::MockProvider::with_script(script));
        let mut providers: HashMap<String, std::sync::Arc<dyn ModelProvider>> = HashMap::new();
        providers.insert(
            "mock".into(),
            std::sync::Arc::clone(&mock) as std::sync::Arc<dyn ModelProvider>,
        );
        let engine = Engine {
            selector: std::sync::Arc::new(crate::testing::StaticSelector::new("m", "mock")),
            providers,
            tools: std::sync::Arc::new(registry),
            store: std::sync::Arc::new(crate::testing::InMemoryStore::default()),
            config,
        };
        (engine, verify, mock)
    }

    #[tokio::test]
    async fn blocked_edit_does_not_arm_the_finish_gate() {
        // Default policy (Ask, no approver): the write is refused and never
        // executes, so the finish gate must not run verify on the user's behalf.
        let (engine, verify, _mock) = finish_engine(
            vec![call("write"), text("done")],
            EngineConfig::default(),
            true,
        );
        let summary = engine.run_task("ship it").await.unwrap();
        assert_eq!(
            verify.0.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a blocked edit must not arm verify"
        );
        assert_eq!(summary.answer, "done", "no verification note either");
        assert_eq!(summary.turns, 2);
    }

    #[tokio::test]
    async fn denied_verify_is_never_run_by_the_finish_gate() {
        // A real edit arms the gate, but `verify` is on the deny list: the
        // gate skips verification and the run ends with a visible note.
        let config = EngineConfig {
            approvals: crate::ApprovalPolicy {
                mode: crate::ApprovalMode::ApproveAll,
                deny: vec!["verify".into()],
                ..crate::ApprovalPolicy::default()
            },
            ..EngineConfig::default()
        };
        let (engine, verify, _mock) =
            finish_engine(vec![call("write"), text("done")], config, true);
        let summary = engine.run_task("ship it").await.unwrap();
        assert_eq!(verify.0.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(
            summary.answer,
            "done\n\n[verification skipped: verify is not permitted in this approval mode]"
        );
        assert_eq!(summary.turns, 2, "the skip ends the run, no feedback turn");
    }

    /// A tool-call turn that also carries non-empty assistant text.
    fn call_with_text(text: &str, name: &str) -> TurnResponse {
        TurnResponse {
            text: text.into(),
            ..call(name)
        }
    }

    #[tokio::test]
    async fn no_verify_tool_runs_exactly_one_self_check() {
        // Self-check on, no verify tool, escalation configured: the first
        // finish earns ONE self-check turn; the second finish ends the run.
        // No failure note and no escalation turn.
        let config = EngineConfig {
            self_check_on_finish: true,
            escalate_on_check_failure: Some("claude-opus-escalated".into()),
            ..approve_all_config()
        };
        let (engine, _verify, mock) = finish_engine(
            vec![call("write"), text("attempt 1"), text("attempt 2")],
            config,
            false,
        );
        let summary = engine.run_task("ship it").await.unwrap();
        assert_eq!(summary.answer, "attempt 2");
        assert_eq!(
            mock.requests().len(),
            3,
            "edit, first answer, exactly one self-check reply"
        );
        assert!(mock.requests().iter().all(|r| r.model == "m"));
    }

    #[tokio::test]
    async fn provider_error_on_the_self_check_turn_keeps_the_answer() {
        let config = EngineConfig {
            self_check_on_finish: true,
            ..approve_all_config()
        };
        // The self-check reply hits the exhausted script: the pre-check
        // answer survives and the run stays Ok.
        let (engine, _verify, _mock) =
            finish_engine(vec![call("write"), text("attempt 1")], config, false);
        let summary = engine.run_task("ship it").await.unwrap();
        assert_eq!(summary.answer, "attempt 1");
    }

    #[tokio::test]
    async fn provider_error_after_the_self_check_turn_propagates() {
        let config = EngineConfig {
            self_check_on_finish: true,
            ..approve_all_config()
        };
        // The self-check reply is a tool turn with text; the turn after it
        // errors (script exhausted). Only the immediate self-check turn may
        // be swallowed, so the error must reach the caller.
        let (engine, _verify, _mock) = finish_engine(
            vec![
                call("write"),
                text("attempt 1"),
                call_with_text("checking", "write"),
            ],
            config,
            false,
        );
        let result = engine.run_task("ship it").await;
        assert!(
            result.is_err(),
            "a provider error on a later turn must propagate, got {result:?}"
        );
    }
}
