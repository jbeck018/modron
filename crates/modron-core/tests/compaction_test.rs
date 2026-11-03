//! Autocompaction acceptance tests (LEDGER X1/X2, PLAN.md §4.10). Fully
//! offline: `MockProvider` scripts the whole run (tool call → long turn →
//! summarizer response → final answer) and the assertions check WHEN
//! compaction fires, that the post-compaction context is actually small
//! (measured ≤ 1.5× a fresh-session baseline for the same task), that
//! accounting includes the compaction call, and that resume reconstructs the
//! compacted state from the event log.

use async_trait::async_trait;
use modron_core::compact;
use modron_core::testing::{InMemoryStore, MockProvider, StaticSelector};
use modron_core::{
    estimate_tokens, CompactionPolicy, Engine, EngineConfig, ModelProvider, Result, Role,
    SessionEvent, StopReason, Tool, ToolCall, ToolCtx, ToolOutcome, ToolRegistry, ToolSpec,
    TurnRequest, TurnResponse, Usage,
};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "echo_tool".into(),
            description: "Echoes a short confirmation.".into(),
            parameters: json!({"type": "object"}),
        }
    }

    async fn execute(&self, _args: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutcome> {
        Ok(ToolOutcome::ok("echo ok"))
    }
}

fn registry() -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(EchoTool));
    Arc::new(registry)
}

/// A realistic multi-sentence task (~270 bytes): big enough that the compact
/// state (a short summary) is smaller than the fresh-session first request,
/// which is what the measured cheapness gate compares against.
const TASK: &str = "Review the login module end to end. List every auth-related edge case \
you find — expired sessions, concurrent logins, token refresh races, lockout behavior — \
and propose which two we should fix first, with a short rationale for each choice.";

/// The scripted summary the mock returns for the compaction (summarizer)
/// call. Kept short: the compact state must be smaller than the task.
const SUMMARY: &str = "TASK: review login module and list its auth edge cases. \
DECISIONS: fix token refresh races first. EDITS: none yet. \
OPEN ITEMS: write the two chosen fixes.";

/// ~7.4KB of chatty turn text (~1850 estimated tokens) to blow past the tiny
/// thresholds used below.
fn long_text() -> String {
    "chatty analysis of the login module. ".repeat(200)
}

fn compaction_config(max_input_tokens: u64, threshold_frac: f64) -> EngineConfig {
    EngineConfig {
        working_dir: PathBuf::from("/nonexistent/modron-compaction-test"),
        system: "You are a test agent.".into(),
        max_input_tokens,
        compaction: CompactionPolicy {
            enabled: true,
            threshold_frac,
            min_interval_turns: 2,
            tail_messages: 1,
            summary_max_bytes: 2048,
            bitmap_compaction: true,
        },
        ..EngineConfig::default()
    }
}

fn engine(
    provider: Arc<dyn ModelProvider>,
    config: EngineConfig,
    store: Arc<InMemoryStore>,
) -> Engine {
    engine_with_model(provider, config, store, "mock-model")
}

/// Same as [`engine`] but pins the selected model id — the bitmap gate keys
/// on the model the selector hands the engine for the post-compaction turn.
fn engine_with_model(
    provider: Arc<dyn ModelProvider>,
    config: EngineConfig,
    store: Arc<InMemoryStore>,
    model: &str,
) -> Engine {
    let mut providers = HashMap::new();
    providers.insert("mock".to_string(), provider);
    Engine {
        selector: Arc::new(StaticSelector::new(model, "mock")),
        providers,
        tools: registry(),
        store,
        config,
    }
}

fn tool_call_response(id: &str, text: &str, usage: Usage) -> TurnResponse {
    TurnResponse {
        text: text.into(),
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: "echo_tool".into(),
            arguments: json!({}),
        }],
        usage,
        stop_reason: StopReason::ToolUse,
    }
}

fn text_response(text: &str, usage: Usage) -> TurnResponse {
    TurnResponse {
        text: text.into(),
        tool_calls: vec![],
        usage,
        stop_reason: StopReason::EndTurn,
    }
}

/// Estimated context of a request as (system + message contents), the same
/// formula the acceptance gate in the ledger uses.
fn request_context_estimate(request: &TurnRequest) -> u64 {
    estimate_tokens(&request.system)
        + request
            .messages
            .iter()
            .map(|m| estimate_tokens(&m.content))
            .sum::<u64>()
}

/// Control run for the measured cheapness gate: the same task shape with a
/// huge budget and compaction disabled. Returns the estimated context of its
/// FIRST request (a fresh session's baseline: system + task only).
async fn control_run_first_request_estimate() -> u64 {
    let long = long_text();
    let provider = Arc::new(
        MockProvider::new()
            .then_response(tool_call_response("call_1", "", Usage::default()))
            .then_response(tool_call_response("call_2", &long, Usage::default()))
            .then_response(text_response(
                "All done: two edge cases chosen.",
                Usage::default(),
            )),
    );
    let mut config = compaction_config(1_000_000, 0.4);
    config.compaction.enabled = false;
    let engine = engine(provider.clone(), config, Arc::new(InMemoryStore::new()));
    engine.run_task(TASK).await.unwrap();

    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        3,
        "control run: turn1 + turn2 + turn3, no compaction call"
    );
    request_context_estimate(&requests[0])
}

/// The scripted acceptance run (LEDGER X1): turn1 tool call → turn2 long
/// chatty text + tool call (blows past the tiny threshold) → compaction
/// summarizer call → turn3 final answer.
fn scripted_provider() -> Arc<MockProvider> {
    let long = long_text();
    Arc::new(
        MockProvider::new()
            .then_response(tool_call_response(
                "call_1",
                "",
                Usage {
                    input_tokens: 100,
                    output_tokens: 20,
                    cached_input_tokens: 0,
                },
            ))
            .then_response(tool_call_response(
                "call_2",
                &long,
                Usage {
                    input_tokens: 3000,
                    output_tokens: 900,
                    cached_input_tokens: 0,
                },
            ))
            .then_response(text_response(
                SUMMARY,
                Usage {
                    input_tokens: 400,
                    output_tokens: 60,
                    cached_input_tokens: 0,
                },
            ))
            .then_response(text_response(
                "All done: two edge cases chosen.",
                Usage {
                    input_tokens: 50,
                    output_tokens: 10,
                    cached_input_tokens: 0,
                },
            )),
    )
}

#[tokio::test]
async fn compaction_fires_once_shrinks_context_and_completes() {
    let provider = scripted_provider();
    let engine = engine(
        provider.clone(),
        compaction_config(4000, 0.4),
        Arc::new(InMemoryStore::new()),
    );

    let summary = engine.run_task(TASK).await.unwrap();

    // (c) The task completes; the compaction call is not a turn.
    assert_eq!(summary.answer, "All done: two edge cases chosen.");
    assert_eq!(
        summary.turns, 3,
        "turn1 + turn2 + turn3 (compaction not counted)"
    );
    assert_eq!(summary.tool_calls, 2);

    let requests = provider.requests();
    assert_eq!(requests.len(), 4, "turn1 + turn2 + compaction call + turn3");

    // The compaction call used the summarizer prompt, no tools, and the
    // small output cap (X2 prompt wired through X1 step 2).
    let compaction_request = &requests[2];
    assert_eq!(compaction_request.system, compact::summarizer_prompt());
    assert!(
        compaction_request.tools.is_empty(),
        "summarizer call carries no tools"
    );
    assert_eq!(compaction_request.max_tokens, 512);
    assert_eq!(compaction_request.messages.len(), 1);
    let older_text = &compaction_request.messages[0].content;
    let long = long_text();
    assert!(
        older_text.contains("[TASK]"),
        "older transcript renders the task"
    );
    assert!(older_text.contains("[TOOL_CALL id=call_1"));
    assert!(older_text.contains("[TOOL_RESULT id=call_2]"));
    assert!(
        older_text.contains(&long),
        "the long turn-2 text is summarized, not tail-kept"
    );

    // X1 step 5: the turn after compaction sees [compact state] + tail.
    // (The only tail candidate was an orphan tool result; it was folded into
    // the summary, so the tail here is empty.)
    let post = &requests[3].messages;
    assert_eq!(
        post.len(),
        1,
        "rebuilt context = compact state + (empty) tail"
    );
    assert_eq!(post[0].role, Role::User);
    assert!(post[0]
        .content
        .starts_with("Conversation compacted; summary of earlier work:\n"));
    assert!(post[0].content.contains(SUMMARY));

    // (b) Measured cheapness gate: post-compaction context estimate
    // (system + messages) ≤ 1.5× the control run's FIRST request estimate.
    let control_estimate = control_run_first_request_estimate().await;
    let post_estimate = request_context_estimate(&requests[3]);
    assert!(
        post_estimate * 2 <= control_estimate * 3,
        "post-compaction estimate {post_estimate} must be ≤ 1.5× fresh-session \
         baseline {control_estimate}"
    );

    // (d) The compaction call's usage is in the run's accounting (scripted
    // usage sums across turn1 + turn2 + compaction + turn3).
    assert_eq!(summary.usage.input_tokens, 100 + 3000 + 400 + 50);
    assert_eq!(summary.usage.output_tokens, 20 + 900 + 60 + 10);

    // (e)+(f) Exactly one Compaction event over the run, placed after the
    // last pre-compaction tool result, with its usage right behind it.
    let events = engine.store.load(summary.session_id).await.unwrap();
    // TurnTrace observability records ride along: one per model turn
    // (4) + the terminal stop-label record.
    assert_eq!(
        events.len(),
        18,
        "Task + 2×(MS,U,M,TR) + 3×TurnTrace + Compaction + U + (MS,U,M) + terminal TurnTrace"
    );
    let compactions = events
        .iter()
        .filter(|event| matches!(event, SessionEvent::Compaction { .. }))
        .count();
    assert_eq!(compactions, 1, "no re-compaction within min_interval");
    let idx = events
        .iter()
        .position(|event| matches!(event, SessionEvent::Compaction { .. }))
        .unwrap();
    assert!(
        matches!(&events[idx - 1], SessionEvent::ToolResult { call_id, .. } if call_id == "call_2"),
        "compaction sits after the last pre-compaction tool result"
    );
    assert!(
        matches!(&events[idx], SessionEvent::Compaction { summary, summarized_events }
            if *summarized_events == 5 && summary == SUMMARY),
        "the Compaction event carries the capped summary and the summarized count"
    );
    assert!(
        matches!(&events[idx + 1], SessionEvent::Usage { usage, .. }
            if usage.input_tokens == 400 && usage.output_tokens == 60),
        "the compaction call's usage is persisted right after the event"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::Message { message, .. }
            if message.content == "All done: two edge cases chosen.")),
        "the final answer is persisted (the terminal TurnTrace follows it)"
    );
}

#[tokio::test]
async fn no_recompaction_within_min_interval_even_when_over_threshold() {
    let long = long_text();
    let provider = Arc::new(
        MockProvider::new()
            .then_response(tool_call_response("c1", "", Usage::default()))
            .then_response(tool_call_response("c2", &long, Usage::default()))
            .then_response(text_response(SUMMARY, Usage::default()))
            .then_response(tool_call_response("c3", &long, Usage::default()))
            .then_response(text_response("finally done", Usage::default())),
    );
    // Threshold 0.35 × 4000 = 1400: turn 2 blows past it and, after the
    // compaction, turn 3 is long again — the context stays over the
    // threshold — but min_interval_turns = 2 blocks a second compaction.
    let engine = engine(
        provider.clone(),
        compaction_config(4000, 0.35),
        Arc::new(InMemoryStore::new()),
    );

    let summary = engine.run_task(TASK).await.unwrap();

    assert_eq!(
        summary.turns, 4,
        "turn1..turn4, compaction call not counted"
    );
    assert_eq!(
        provider.requests().len(),
        5,
        "4 main turns + 1 compaction call"
    );
    let events = engine.store.load(summary.session_id).await.unwrap();
    let compactions = events
        .iter()
        .filter(|event| matches!(event, SessionEvent::Compaction { .. }))
        .count();
    assert_eq!(
        compactions, 1,
        "min_interval must block a second compaction while still over threshold"
    );
    assert_eq!(summary.answer, "finally done", "task still completes");
}

#[tokio::test]
async fn resume_reconstructs_compact_state_from_events() {
    let provider = scripted_provider();
    let store = Arc::new(InMemoryStore::new());
    let engine = engine(
        provider.clone(),
        compaction_config(4000, 0.4),
        store.clone(),
    );
    let summary = engine.run_task(TASK).await.unwrap();
    let in_run_compact_message = provider.requests()[3].messages[0].content.clone();

    // Resume the SAME session with a fresh provider/script.
    let provider2 = Arc::new(MockProvider::new().then_text("resumed answer"));
    let mut providers: HashMap<String, Arc<dyn ModelProvider>> = HashMap::new();
    providers.insert("mock".to_string(), provider2.clone());
    let engine2 = Engine {
        selector: Arc::new(StaticSelector::new("mock-model", "mock")),
        providers,
        tools: registry(),
        store,
        config: compaction_config(4000, 0.4),
    };
    let summary2 = engine2
        .run_task_resuming(summary.session_id, "keep going")
        .await
        .unwrap();

    assert_eq!(
        summary2.session_id, summary.session_id,
        "resume continues the SAME session"
    );
    assert_eq!(summary2.answer, "resumed answer");

    let requests = provider2.requests();
    assert_eq!(requests.len(), 1);
    let messages = &requests[0].messages;
    assert_eq!(
        messages.len(),
        3,
        "compact state + events after the compaction (final message) + new task"
    );
    assert_eq!(
        messages[0].content, in_run_compact_message,
        "resume rebuilds the byte-identical compact-state message from the Compaction event"
    );
    assert_eq!(messages[1].role, Role::Assistant);
    assert_eq!(messages[1].content, "All done: two edge cases chosen.");
    assert_eq!(messages[2].role, Role::User);
    assert_eq!(messages[2].content, "keep going");
}

#[tokio::test]
async fn compaction_disabled_never_fires() {
    let provider = Arc::new(
        MockProvider::new()
            .then_response(tool_call_response("c1", "", Usage::default()))
            .then_text("done without compaction"),
    );
    // Threshold 0.01 × 4000 would fire immediately if enabled.
    let mut config = compaction_config(4000, 0.01);
    config.compaction.enabled = false;
    let engine = engine(provider.clone(), config, Arc::new(InMemoryStore::new()));

    let summary = engine.run_task(TASK).await.unwrap();
    assert_eq!(summary.answer, "done without compaction");
    let events = engine.store.load(summary.session_id).await.unwrap();
    assert!(!events
        .iter()
        .any(|event| matches!(event, SessionEvent::Compaction { .. })));
}

/// X0 guard: a Phase-0-style session log (no Compaction events) still loads,
/// and the new variant roundtrips in the existing snake_case tag style.
#[test]
fn phase0_session_events_still_load_and_compaction_roundtrips() {
    let phase0_lines = [
        r#"{"type":"task","text":"add a feature"}"#,
        r#"{"type":"model_selected","model":"m1","reason":"default"}"#,
        r#"{"type":"message","parent_id":null,"message":{"role":"user","content":"hello","tool_calls":[]}}"#,
        r#"{"type":"message","parent_id":null,"message":{"role":"assistant","content":"running","tool_calls":[{"id":"c1","name":"bash","arguments":{"command":"ls"}}]}}"#,
        r#"{"type":"tool_result","call_id":"c1","outcome":{"content":"files","is_error":false}}"#,
        r#"{"type":"usage","model":"m1","usage":{"input_tokens":10,"output_tokens":5,"cached_input_tokens":2}}"#,
    ];
    for line in phase0_lines {
        let event: SessionEvent = serde_json::from_str(line)
            .unwrap_or_else(|error| panic!("Phase-0 event failed to load: {error}\n{line}"));
        let reserialized = serde_json::to_string(&event).unwrap();
        serde_json::from_str::<SessionEvent>(&reserialized)
            .unwrap_or_else(|error| panic!("re-serialization broke an old event: {error}"));
    }

    // The new variant uses the same tag style and roundtrips.
    let compaction = SessionEvent::Compaction {
        summary: "s".into(),
        summarized_events: 4,
    };
    assert_eq!(
        serde_json::to_string(&compaction).unwrap(),
        r#"{"type":"compaction","summary":"s","summarized_events":4}"#
    );
    assert!(matches!(
        serde_json::from_str::<SessionEvent>(
            r#"{"type":"compaction","summary":"s","summarized_events":4}"#
        ),
        Ok(SessionEvent::Compaction { .. })
    ));

    // EngineConfig's default carries the Phase-1a compaction defaults.
    let config = EngineConfig::default();
    assert_eq!(
        config.compaction,
        CompactionPolicy {
            enabled: true,
            threshold_frac: 0.8,
            min_interval_turns: 4,
            tail_messages: 6,
            summary_max_bytes: 2048,
            bitmap_compaction: true,
        }
    );
}

/// Phase 5i: the SELECTED model's window drives the compaction trigger —
/// a 128k-window model compacts at 0.8×128k even when the global cap is
/// 180k. This is the live finding from the 105k-token free-model run.
#[tokio::test]
async fn compaction_uses_the_model_window_not_the_global_cap() {
    use modron_core::testing::{MockProvider, StaticSelector};
    use modron_core::{CompactionPolicy, SessionEvent, SessionStore};

    // A model with a small window; the global cap stays large.
    let selector = StaticSelector::new("small-window-model", "mock");
    // Fake its window via a ModelChoice-carrying selector: StaticSelector
    // reports context_window None, so use a custom wrapper.
    struct WindowedSelector(StaticSelector);
    impl modron_core::ModelSelector for WindowedSelector {
        fn select(&self, req: &modron_core::SelectRequest) -> modron_core::ModelChoice {
            let mut choice = self.0.select(req);
            choice.context_window = Some(10_000); // tiny window
            choice
        }
    }

    // Turn 1: large assistant text + a tool call → the transcript crosses
    // 0.8 × 10k, so turn 2 must compact (window-driven, not global-cap).
    // Script: turn 1 (tool call, huge text), then the SUMMARIZER response
    // (the compaction pass consumes it), then turn 2's final answer.
    let provider = MockProvider::new()
        .then_response(TurnResponse {
            text: "y".repeat(40_000),
            tool_calls: vec![modron_core::ToolCall {
                id: "c1".into(),
                name: "noop".into(),
                arguments: serde_json::json!({}),
            }],
            usage: Usage {
                input_tokens: 9_000,
                output_tokens: 100,
                cached_input_tokens: 0,
            },
            stop_reason: StopReason::ToolUse,
        })
        .then_response(TurnResponse {
            text: "TASK: force compaction\nDECISIONS: none\nEDITS: none\nOPEN ITEMS: none".into(),
            tool_calls: Vec::new(),
            usage: Usage {
                input_tokens: 500,
                output_tokens: 50,
                cached_input_tokens: 0,
            },
            stop_reason: StopReason::EndTurn,
        })
        .then_response(TurnResponse {
            text: "done".into(),
            tool_calls: Vec::new(),
            usage: Usage {
                input_tokens: 200,
                output_tokens: 10,
                cached_input_tokens: 0,
            },
            stop_reason: StopReason::EndTurn,
        });

    let mut registry = modron_core::ToolRegistry::new();
    struct Noop;
    #[async_trait::async_trait]
    impl modron_core::Tool for Noop {
        fn spec(&self) -> modron_core::ToolSpec {
            modron_core::ToolSpec {
                name: "noop".into(),
                description: "does nothing".into(),
                parameters: serde_json::json!({"type": "object"}),
            }
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &modron_core::ToolCtx,
        ) -> modron_core::Result<modron_core::ToolOutcome> {
            Ok(modron_core::ToolOutcome::ok("ok"))
        }
    }
    registry.register(std::sync::Arc::new(Noop));

    let store: Arc<dyn SessionStore> = Arc::new(modron_core::testing::InMemoryStore::default());
    let engine = modron_core::Engine {
        selector: Arc::new(WindowedSelector(selector)),
        providers: [(
            "mock".to_string(),
            Arc::new(provider) as Arc<dyn modron_core::ModelProvider>,
        )]
        .into_iter()
        .collect(),
        tools: Arc::new(registry),
        store: Arc::clone(&store),
        config: modron_core::EngineConfig {
            working_dir: tempfile::tempdir().unwrap().path().to_path_buf(),
            max_input_tokens: 180_000, // global cap LARGE — window must win
            compaction: CompactionPolicy {
                enabled: true,
                threshold_frac: 0.8,
                min_interval_turns: 0,
                tail_messages: 1, // the 40k message must NOT survive in the tail
                summary_max_bytes: 2048,
                bitmap_compaction: true,
            },
            ..modron_core::EngineConfig::default()
        },
    };

    let summary = engine.run_task("force compaction").await.unwrap();
    let events = store.load(summary.session_id).await.unwrap();
    let compactions = events
        .iter()
        .filter(|e| matches!(e, SessionEvent::Compaction { .. }))
        .count();
    assert_eq!(
        compactions, 1,
        "compaction fired against the 10k model window despite the 180k global cap"
    );
}

/// Bitmap compaction gate (oh-my-pi pattern): the pruned older transcript
/// rides the compact-state message as ONE pixel-font PNG attachment, but
/// only when the model selected for the NEXT turn is vision-capable — a
/// text-only model would pay image tokens for pixels it cannot read. The
/// text summary is kept in every branch (the bitmap is additive). The
/// kill-switch (`bitmap_compaction: false`) must also suppress it.
#[tokio::test]
async fn compaction_attaches_bitmap_for_vision_models_only() {
    // (a) Vision model: exactly one image/png attachment on the compact
    // state, with the text summary still in place.
    let provider = scripted_provider();
    let engine = engine_with_model(
        provider.clone(),
        compaction_config(4000, 0.4),
        Arc::new(InMemoryStore::new()),
        "claude-opus-5-5",
    );
    engine.run_task(TASK).await.unwrap();
    let requests = provider.requests();
    assert_eq!(requests.len(), 4, "turn1 + turn2 + compaction call + turn3");
    let compact = &requests[3].messages[0];
    assert!(
        compact
            .content
            .starts_with("Conversation compacted; summary of earlier work:\n"),
        "the text summary always stays"
    );
    assert!(compact.content.contains(SUMMARY));
    assert_eq!(compact.attachments.len(), 1, "vision model gets the bitmap");
    match &compact.attachments[0] {
        modron_core::Attachment::Image { media_type, data } => {
            assert_eq!(media_type, "image/png");
            let bytes = modron_core::b64::decode(data).expect("valid base64 body");
            assert_eq!(&bytes[..4], b"\x89PNG", "body is PNG bytes");
        }
        other => panic!("expected an image attachment, got {other:?}"),
    }

    // (b) Non-vision model: same script, same policy — no attachment.
    let provider = scripted_provider();
    let engine = engine_with_model(
        provider.clone(),
        compaction_config(4000, 0.4),
        Arc::new(InMemoryStore::new()),
        "nonvision-model",
    );
    engine.run_task(TASK).await.unwrap();
    let requests = provider.requests();
    let compact = &requests[3].messages[0];
    assert!(
        compact.content.contains(SUMMARY),
        "summary text is unaffected"
    );
    assert!(
        compact.attachments.is_empty(),
        "non-vision model must not receive a bitmap"
    );

    // (c) Kill-switch: `bitmap_compaction = false` suppresses the bitmap
    // even for a vision-capable model.
    let provider = scripted_provider();
    let mut config = compaction_config(4000, 0.4);
    config.compaction.bitmap_compaction = false;
    let engine = engine_with_model(
        provider.clone(),
        config,
        Arc::new(InMemoryStore::new()),
        "claude-opus-5-5",
    );
    engine.run_task(TASK).await.unwrap();
    let compact = &provider.requests()[3].messages[0];
    assert!(
        compact.attachments.is_empty(),
        "the policy kill-switch must suppress the bitmap"
    );
}
