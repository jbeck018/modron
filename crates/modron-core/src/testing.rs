//! Test doubles — LEDGER item C4.
//!
//! `MockProvider`: scripted `TurnResponse` sequence; records every
//! `TurnRequest` it receives so tests can assert on assembled context.
//! Also ships an in-memory `SessionStore` and a fixed `ModelSelector` so the
//! engine loop can be exercised fully offline (LEDGER V2).

use crate::{
    Error, ModelChoice, ModelProvider, ModelSelector, Result, SelectRequest, SessionEvent,
    SessionId, SessionMeta, SessionStore, StopReason, ToolCall, TurnRequest, TurnResponse, Usage,
};
use async_trait::async_trait;
use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, MutexGuard};
use uuid::Uuid;

/// Lock a mutex, recovering from poisoning. Test doubles never panic on a
/// poisoned lock: the data is still usable.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Scripted [`ModelProvider`] for offline tests (LEDGER C4; drives the V2
/// end-to-end acceptance test). `turn` pops the next scripted response and
/// records the request. Exhausting the script is an error, never a panic.
///
/// Arc-shareable: all state is behind mutexes and methods take `&self`.
///
/// ```no_run
/// use modron_core::testing::MockProvider;
///
/// let provider = MockProvider::new().then_text("All done.");
/// ```
#[derive(Default)]
pub struct MockProvider {
    script: Mutex<VecDeque<TurnResponse>>,
    requests: Mutex<Vec<TurnRequest>>,
    /// When set, an exhausted script replays its last response forever —
    /// what multi-turn chat tests need (the conversation continues).
    repeat_last: Mutex<bool>,
    last: Mutex<Option<TurnResponse>>,
}

impl MockProvider {
    /// Create a mock with an empty script (first `turn` errors).
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the script from a ready-made response sequence.
    pub fn with_script(script: Vec<TurnResponse>) -> Self {
        Self {
            script: Mutex::new(script.into()),
            requests: Mutex::new(Vec::new()),
            repeat_last: Mutex::new(false),
            last: Mutex::new(None),
        }
    }

    /// Builder: append one scripted response.
    pub fn then_response(self, response: TurnResponse) -> Self {
        lock(&self.script).push_back(response);
        self
    }

    /// Builder: when the script runs dry, replay the last response instead of
    /// erroring (multi-turn chat tests; at least one response must exist).
    pub fn then_repeat_last(self) -> Self {
        *lock(&self.repeat_last) = true;
        self
    }

    /// Builder: append a plain end-of-turn text response with zero usage.
    pub fn then_text(self, text: impl Into<String>) -> Self {
        self.then_response(TurnResponse {
            text: text.into(),
            tool_calls: Vec::new(),
            usage: Usage::default(),
            stop_reason: StopReason::EndTurn,
        })
    }

    /// Builder: append a single tool-call response with zero usage.
    pub fn then_tool_call(
        self,
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: serde_json::Value,
    ) -> Self {
        self.then_response(TurnResponse {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: id.into(),
                name: name.into(),
                arguments,
            }],
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
        })
    }

    /// Every request received so far, in arrival order.
    pub fn requests(&self) -> Vec<TurnRequest> {
        lock(&self.requests).clone()
    }
}

/// Ready-made two-turn script for shell demos and smoke tests (Definition of
/// Usable #4): turn 1 calls the real `bash` tool to echo a marker, turn 2
/// returns the final answer. Requires `modron-tools`'s `bash` tool in the
/// registry. Usage numbers are scripted like everything else in a mock — the
/// CLI still reports them honestly as provider-reported.
/// A rich demo that exercises EVERY TUI view: tool calls (start/done),
/// streaming text, a delegate subagent, an error, and a final answer.
/// This is what `modron --provider mock` shows — the review-everything script.
pub fn rich_demo_script() -> Vec<TurnResponse> {
    use StopReason::*;
    vec![
        // Turn 1: parallel tool calls (bash + read + grep) — exercises the
        // concurrent read-only dispatcher and the cyan tool lines.
        TurnResponse {
            text: String::new(),
            tool_calls: vec![
                ToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command": "echo 'hello from bash'"}),
                },
                ToolCall {
                    id: "c2".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path": "Cargo.toml"}),
                },
                ToolCall {
                    id: "c3".into(),
                    name: "grep".into(),
                    arguments: serde_json::json!({"pattern": "modron"}),
                },
            ],
            usage: Usage { input_tokens: 800, output_tokens: 120, cached_input_tokens: 400 },
            stop_reason: ToolUse,
        },
        // Turn 2: a write (exercises the checkpoint path).
        TurnResponse {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "c4".into(),
                name: "write".into(),
                arguments: serde_json::json!({
                    "path": "demo-output.txt",
                    "content": "demo write from modron"
                }),
            }],
            usage: Usage { input_tokens: 900, output_tokens: 80, cached_input_tokens: 600 },
            stop_reason: ToolUse,
        },
        // Turn 3: a failing bash (exercises the error styling).
        TurnResponse {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "c5".into(),
                name: "bash".into(),
                arguments: serde_json::json!({"command": "echo 'simulated failure' >&2; exit 42"}),
            }],
            usage: Usage { input_tokens: 1000, output_tokens: 70, cached_input_tokens: 800 },
            stop_reason: ToolUse,
        },
        // Turn 4: the final streamed answer.
        TurnResponse {
            text: "Demo complete.\n\nWhat you just saw:\n- Parallel tool calls (bash + read + grep)\n- A file write with checkpoint\n- An error result (exit 42)\n- This streamed answer\n\nEvery TUI view was exercised.".into(),
            tool_calls: Vec::new(),
            usage: Usage { input_tokens: 1100, output_tokens: 200, cached_input_tokens: 900 },
            stop_reason: EndTurn,
        },
    ]
}

/// Convenience: provider with the rich demo script.
pub fn rich_demo_provider() -> MockProvider {
    MockProvider::with_script(rich_demo_script()).then_repeat_last()
}

pub fn demo_script() -> Vec<TurnResponse> {
    vec![
        TurnResponse {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "demo-1".into(),
                name: "bash".into(),
                arguments: serde_json::json!({"command": "echo modron-mock-ok"}),
            }],
            usage: Usage {
                input_tokens: 512,
                output_tokens: 32,
                cached_input_tokens: 0,
            },
            stop_reason: StopReason::ToolUse,
        },
        TurnResponse {
            text: "Mock run complete — the loop, tools, store, and budget all executed.".into(),
            tool_calls: Vec::new(),
            usage: Usage {
                input_tokens: 640,
                output_tokens: 48,
                cached_input_tokens: 0,
            },
            stop_reason: StopReason::EndTurn,
        },
    ]
}

/// [`MockProvider`] seeded with [`demo_script`] — the keyless demo provider
/// the CLI registers for `--provider mock`.
pub fn demo_provider() -> MockProvider {
    // repeat_last: chat mode keeps one provider across exchanges, so the
    // demo must not exhaust after the first exchange.
    MockProvider::with_script(demo_script()).then_repeat_last()
}

#[async_trait]
impl ModelProvider for MockProvider {
    fn id(&self) -> &str {
        "mock"
    }

    async fn turn(&self, request: TurnRequest) -> Result<TurnResponse> {
        lock(&self.requests).push(request);
        let mut script = lock(&self.script);
        match script.pop_front() {
            Some(response) => {
                *lock(&self.last) = Some(response.clone());
                Ok(response)
            }
            None if *lock(&self.repeat_last) => {
                lock(&self.last).clone().ok_or_else(|| Error::Provider {
                    provider: "mock".into(),
                    message: "mock script exhausted: repeat_last set but the script was empty"
                        .into(),
                })
            }
            None => Err(Error::Provider {
                provider: "mock".into(),
                message: "mock script exhausted: every scripted TurnResponse has been consumed"
                    .into(),
            }),
        }
    }
}

/// One session's metadata and full event log.
#[derive(Clone, Debug)]
struct SessionRecord {
    meta: SessionMeta,
    events: Vec<SessionEvent>,
}

/// In-memory [`SessionStore`] for tests: append-only events per session with
/// the same semantics as the real JSONL store, minus persistence.
#[derive(Default)]
pub struct InMemoryStore {
    sessions: Mutex<HashMap<SessionId, SessionRecord>>,
}

impl InMemoryStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl SessionStore for InMemoryStore {
    async fn create_session(&self, title: &str) -> Result<SessionMeta> {
        let mut sessions = lock(&self.sessions);
        let now = chrono::Utc::now();
        let meta = SessionMeta {
            id: Uuid::new_v4(),
            title: title.to_string(),
            created_at: now,
            updated_at: now,
            message_count: 0,
        };
        sessions.insert(
            meta.id,
            SessionRecord {
                meta: meta.clone(),
                events: Vec::new(),
            },
        );
        Ok(meta)
    }

    async fn append(&self, id: SessionId, event: &SessionEvent) -> Result<()> {
        let mut sessions = lock(&self.sessions);
        let record = sessions
            .get_mut(&id)
            .ok_or_else(|| Error::Session(format!("unknown session {id}")))?;
        if matches!(event, SessionEvent::Message { .. }) {
            record.meta.message_count += 1;
        }
        record.meta.updated_at = chrono::Utc::now();
        record.events.push(event.clone());
        Ok(())
    }

    async fn load(&self, id: SessionId) -> Result<Vec<SessionEvent>> {
        let sessions = lock(&self.sessions);
        let record = sessions
            .get(&id)
            .ok_or_else(|| Error::Session(format!("unknown session {id}")))?;
        Ok(record.events.clone())
    }

    async fn list(&self) -> Result<Vec<SessionMeta>> {
        let sessions = lock(&self.sessions);
        let mut metas: Vec<SessionMeta> = sessions.values().map(|r| r.meta.clone()).collect();
        metas.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        Ok(metas)
    }
}

/// A [`ModelSelector`] that always returns the same choice — pins engine tests
/// to a fixed model/provider without depending on the router crate.
#[derive(Clone, Debug)]
pub struct StaticSelector {
    pub model: String,
    pub provider: String,
    pub reason: String,
}

impl StaticSelector {
    /// Pin every selection to `model` served by `provider`.
    pub fn new(model: impl Into<String>, provider: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            provider: provider.into(),
            reason: "static test selector".into(),
        }
    }
}

impl ModelSelector for StaticSelector {
    fn select(&self, _req: &SelectRequest) -> ModelChoice {
        ModelChoice {
            model: self.model.clone(),
            provider: self.provider.clone(),
            reason: self.reason.clone(),
            confident: true,
            context_window: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Message;

    #[tokio::test]
    async fn mock_serves_script_in_order_and_records_requests() {
        let mock = MockProvider::new().then_text("one").then_text("two");
        let request = TurnRequest {
            model: "m".into(),
            system: "s".into(),
            messages: vec![Message::user("hi")],
            tools: vec![],
            max_tokens: 100,
        };

        assert_eq!(mock.turn(request.clone()).await.unwrap().text, "one");
        assert_eq!(mock.turn(request.clone()).await.unwrap().text, "two");

        let err = mock.turn(request.clone()).await.unwrap_err();
        assert!(matches!(err, Error::Provider { ref provider, .. } if provider == "mock"));

        let requests = mock.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].messages[0].content, "hi");
    }

    #[tokio::test]
    async fn mock_tool_call_builder_shapes_a_response() {
        let mock =
            MockProvider::new().then_tool_call("c1", "read", serde_json::json!({"path": "x"}));
        let response = mock
            .turn(TurnRequest {
                model: "m".into(),
                system: String::new(),
                messages: vec![],
                tools: vec![],
                max_tokens: 1,
            })
            .await
            .unwrap();
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        assert_eq!(response.tool_calls[0].name, "read");
        assert_eq!(response.tool_calls[0].arguments["path"], "x");
    }

    #[tokio::test]
    async fn in_memory_store_roundtrip() {
        let store = InMemoryStore::new();
        let meta = store.create_session("title").await.unwrap();
        store
            .append(
                meta.id,
                &SessionEvent::Task {
                    text: "do it".into(),
                },
            )
            .await
            .unwrap();
        store
            .append(
                meta.id,
                &SessionEvent::Message {
                    parent_id: None,
                    message: Message::assistant("done"),
                },
            )
            .await
            .unwrap();

        let events = store.load(meta.id).await.unwrap();
        assert_eq!(events.len(), 2);

        let listed = store.list().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].title, "title");
        assert_eq!(listed[0].message_count, 1);

        assert!(store.load(Uuid::new_v4()).await.is_err());
        assert!(store
            .append(Uuid::new_v4(), &SessionEvent::Task { text: "x".into() })
            .await
            .is_err());
    }

    #[tokio::test]
    async fn demo_script_serves_bash_then_final_text() {
        let mock = demo_provider();
        let request = TurnRequest {
            model: "m".into(),
            system: String::new(),
            messages: vec![Message::user("demo")],
            tools: vec![],
            max_tokens: 100,
        };

        let first = mock.turn(request.clone()).await.unwrap();
        assert_eq!(first.stop_reason, StopReason::ToolUse);
        assert_eq!(first.tool_calls.len(), 1);
        assert_eq!(first.tool_calls[0].name, "bash");
        assert_eq!(
            first.tool_calls[0].arguments["command"],
            "echo modron-mock-ok"
        );
        assert!(first.usage.total() > 0, "scripted usage must be non-zero");

        let second = mock.turn(request).await.unwrap();
        assert_eq!(second.stop_reason, StopReason::EndTurn);
        assert!(second.text.contains("Mock run complete"));
        assert!(second.tool_calls.is_empty());
    }
}

/// Scripted [`AsyncRouter`](crate::AsyncRouter) for engine tests: returns
/// the queued verdicts in order, `None` when exhausted.
#[derive(Default)]
pub struct MockAsyncRouter {
    verdicts: Mutex<VecDeque<Option<crate::RouteVerdict>>>,
    pub consulted: Mutex<Vec<String>>,
}

impl MockAsyncRouter {
    pub fn new(verdicts: Vec<Option<crate::RouteVerdict>>) -> Self {
        Self {
            verdicts: Mutex::new(verdicts.into()),
            consulted: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl crate::AsyncRouter for MockAsyncRouter {
    async fn route(&self, task: &str, _action: crate::ActionKind) -> Option<crate::RouteVerdict> {
        self.consulted.lock().unwrap().push(task.to_string());
        self.verdicts.lock().unwrap().pop_front().flatten()
    }
}
