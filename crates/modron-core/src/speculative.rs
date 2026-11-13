//! Speculative execution (oh-my-pi pattern): while a provider turn is still
//! streaming, a completed tool-call item for an idempotent read-only tool
//! executes immediately, so the dispatch after the turn reuses the result
//! instead of re-running the tool. Extracted from `engine.rs` (2026-11-12
//! architecture audit); the engine's run loop arms one
//! [`SpeculativeDriver`] per streamed turn and drains it when the turn ends.

use crate::{DeltaSink, Engine, EngineConfig, StreamDelta, ToolCall, ToolCtx, ToolOutcome};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Idempotent, side-effect-free tools eligible for speculative execution
/// (started mid-stream when the provider completes a tool-call item early,
/// reused by the dispatch once the turn finishes — oh-my-pi pattern).
/// bash/write/edit stay out: side effects must never run on a call the
/// model might still revise.
pub(crate) const SPECULATIVE_TOOLS: [&str; 3] = ["read", "grep", "glob"];

/// Identity key for a speculative result: (tool, exact wire args).
pub(crate) fn speculative_key(call: &ToolCall) -> String {
    format!("{}:{}", call.name, call.arguments)
}

/// Whether the speculative driver may run for this config. Gated OFF when
/// any observer or gate would be bypassed: pre_tool/post_tool hooks, the
/// on_tool_event sink, and the --deny list all must keep seeing every
/// execution.
pub(crate) fn allowed_for(config: &EngineConfig) -> bool {
    config.hooks.pre_tool.is_none()
        && config.hooks.post_tool.is_none()
        && config.on_tool_event.is_none()
        && !config
            .approvals
            .deny
            .iter()
            .any(|d| SPECULATIVE_TOOLS.contains(&d.as_str()))
}

/// Per-turn speculative-execution driver. Owns the delta pump task, the
/// shared result map, and the in-flight execution handles so every
/// speculation is joined before the drain in [`Self::shutdown`].
pub(crate) struct SpeculativeDriver {
    /// Shared (tool, args) → outcome map. Placeholder-inserted synchronously
    /// when a call is offered (duplicate items for the same call must not
    /// spawn duplicate executions), overwritten with the real outcome by the
    /// execution task.
    results: Arc<Mutex<HashMap<String, ToolOutcome>>>,
    /// The pump task; exits — returning the in-flight handles — when the
    /// provider drops its side of the delta channel.
    pump: tokio::task::JoinHandle<Vec<tokio::task::JoinHandle<()>>>,
}

impl SpeculativeDriver {
    /// Spawn the driver for one streaming turn: forwards every non-tool-call
    /// delta to the sink, and when the provider completes a tool-call item
    /// mid-stream, immediately executes idempotent reads so the dispatch can
    /// reuse the result instead of re-running the tool. The channel side
    /// stays with the caller (the provider consumes it during the turn).
    pub(crate) fn spawn(
        engine: Arc<Engine>,
        session_id: crate::SessionId,
        sink: Arc<DeltaSink>,
        mut rx: tokio::sync::mpsc::UnboundedReceiver<StreamDelta>,
    ) -> Self {
        let results: Arc<Mutex<HashMap<String, ToolOutcome>>> = Arc::default();
        let driver_map = Arc::clone(&results);
        let pump = tokio::spawn(async move {
            let mut in_flight: Vec<tokio::task::JoinHandle<()>> = Vec::new();
            while let Some(delta) = rx.recv().await {
                match delta {
                    StreamDelta::ToolCallReady(call) => {
                        if !SPECULATIVE_TOOLS.contains(&call.name.as_str()) {
                            continue;
                        }
                        let key = speculative_key(&call);
                        // Placeholder first: duplicate items for the same
                        // call must not spawn duplicate executions.
                        {
                            let mut map = driver_map.lock().unwrap_or_else(|e| e.into_inner());
                            if map.contains_key(&key) {
                                continue;
                            }
                            map.insert(key.clone(), ToolOutcome::err(String::new()));
                        }
                        let engine = Arc::clone(&engine);
                        let map = Arc::clone(&driver_map);
                        in_flight.push(tokio::spawn(async move {
                            let name = call.name.clone();
                            let ctx = ToolCtx {
                                sandbox: engine.config.sandbox,
                                working_dir: engine.config.working_dir.clone(),
                                engine: Some(Arc::clone(&engine)),
                                session_id: Some(session_id),
                            };
                            let outcome = match engine.tools.get(&name) {
                                Some(tool) => tool
                                    .execute(call.arguments.clone(), &ctx)
                                    .await
                                    .unwrap_or_else(|e| ToolOutcome::err(e.to_string())),
                                None => ToolOutcome::err("unknown tool"),
                            };
                            map.lock().unwrap().insert(speculative_key(&call), outcome);
                        }));
                    }
                    other => sink(other),
                }
            }
            in_flight
        });
        Self { results, pump }
    }

    /// Join the driver after the streaming turn finished (the provider has
    /// dropped its channel side): wait for the pump, then every in-flight
    /// execution, then drain the result map. The drained (key, outcome)
    /// pairs are the mid-stream results the dispatch may reuse — completed
    /// outcomes, never placeholders, because every execution was joined
    /// before the drain.
    pub(crate) async fn shutdown(self) -> HashMap<String, ToolOutcome> {
        // A panicking pump must not silently discard in-flight handles: they
        // would keep running detached while drain() hands out placeholders
        // (cycle-2 audit M6; the empty placeholder is treated as a miss by
        // dispatch_tool_call, so the real tool re-executes).
        let in_flight = match self.pump.await {
            Ok(handles) => handles,
            Err(err) => {
                tracing::warn!(error = %err, "speculative pump panicked - its in-flight tools were lost");
                Vec::new()
            }
        };
        for handle in in_flight {
            let _ = handle.await;
        }
        self.results
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .collect()
    }
}

/// Plain forwarder: speculation disabled (hooks, event sink, or deny list
/// present) — every delta goes to the sink untouched.
pub(crate) fn spawn_forwarder(
    sink: Arc<DeltaSink>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<StreamDelta>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(delta) = rx.recv().await {
            sink(delta);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{InMemoryStore, StaticSelector};
    use crate::{Tool, ToolRegistry, ToolSpec};
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Glob stub counting executions (the engine-level speculative tests
    /// use the same shape).
    struct CountingGlob(Arc<AtomicUsize>);
    #[async_trait]
    impl Tool for CountingGlob {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "glob".into(),
                description: "counting glob".into(),
                parameters: json!({"type": "object"}),
            }
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolCtx,
        ) -> crate::Result<ToolOutcome> {
            self.0.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            Ok(ToolOutcome::ok("one file"))
        }
    }

    fn glob_engine(counts: Arc<AtomicUsize>) -> Engine {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(CountingGlob(counts)));
        Engine {
            selector: Arc::new(StaticSelector::new("m", "mock")),
            providers: HashMap::new(),
            tools: Arc::new(registry),
            store: Arc::new(InMemoryStore::default()),
            config: EngineConfig::default(),
        }
    }

    fn glob_call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "glob".into(),
            arguments: json!({"pattern": "*.rs"}),
        }
    }

    /// Drive one driver to completion: spawn it, offer the deltas, drop the
    /// channel, shut down. Returns (drained results, deltas the sink saw).
    async fn run_driver(
        engine: Engine,
        offered: Vec<StreamDelta>,
    ) -> (HashMap<String, ToolOutcome>, Vec<StreamDelta>) {
        let seen: Arc<Mutex<Vec<StreamDelta>>> = Arc::default();
        let sink_seen = Arc::clone(&seen);
        let sink: Arc<DeltaSink> = Arc::new(move |delta| {
            sink_seen.lock().unwrap().push(delta);
        });
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<StreamDelta>();
        let driver = SpeculativeDriver::spawn(Arc::new(engine), uuid::Uuid::new_v4(), sink, rx);
        for delta in offered {
            let _ = tx.send(delta);
        }
        drop(tx);
        let results = driver.shutdown().await;
        let sunk = seen.lock().unwrap().drain(..).collect();
        (results, sunk)
    }

    #[tokio::test]
    async fn speculative_call_runs_once_and_drains_completed() {
        let counts = Arc::new(AtomicUsize::new(0));
        let (results, sunk) = run_driver(
            glob_engine(Arc::clone(&counts)),
            vec![
                StreamDelta::Text("checking ".into()),
                StreamDelta::ToolCallReady(glob_call("s1")),
            ],
        )
        .await;
        assert_eq!(
            counts.load(Ordering::SeqCst),
            1,
            "the mid-stream call executed exactly once"
        );
        let key = speculative_key(&glob_call("s1"));
        let outcome = results
            .get(&key)
            .expect("drained result keyed by (tool, args)");
        assert!(!outcome.is_error);
        assert_eq!(outcome.content, "one file");
        // The outcome is the completed one, not the empty placeholder the
        // offer inserts — shutdown joins every execution before draining.
        // Text deltas ride through to the sink; ToolCallReady never does.
        assert_eq!(sunk.len(), 1);
        assert!(matches!(&sunk[0], StreamDelta::Text(t) if t == "checking "));
    }

    #[tokio::test]
    async fn duplicate_offers_execute_once() {
        // Duplicate items for the same call (even under different call ids —
        // the identity key is (tool, exact wire args)) must not spawn
        // duplicate executions: the placeholder is inserted synchronously at
        // offer time.
        let counts = Arc::new(AtomicUsize::new(0));
        let (results, _) = run_driver(
            glob_engine(Arc::clone(&counts)),
            vec![
                StreamDelta::ToolCallReady(glob_call("s1")),
                StreamDelta::ToolCallReady(glob_call("s2")),
            ],
        )
        .await;
        assert_eq!(counts.load(Ordering::SeqCst), 1);
        assert_eq!(results.len(), 1);
    }

    #[tokio::test]
    async fn distinct_arguments_execute_separately() {
        let counts = Arc::new(AtomicUsize::new(0));
        let mut other = glob_call("s2");
        other.arguments = json!({"pattern": "*.md"});
        let (results, _) = run_driver(
            glob_engine(Arc::clone(&counts)),
            vec![
                StreamDelta::ToolCallReady(glob_call("s1")),
                StreamDelta::ToolCallReady(other),
            ],
        )
        .await;
        assert_eq!(counts.load(Ordering::SeqCst), 2);
        assert_eq!(results.len(), 2);
    }

    #[tokio::test]
    async fn non_speculative_tool_never_executes_or_reaches_the_sink() {
        // bash is outside SPECULATIVE_TOOLS: no speculative execution, no
        // result entry, and the ToolCallReady delta is intercepted — the
        // sink never sees the variant.
        let counts = Arc::new(AtomicUsize::new(0));
        let bash = ToolCall {
            id: "b1".into(),
            name: "bash".into(),
            arguments: json!({"command": "echo hi"}),
        };
        let (results, sunk) = run_driver(
            glob_engine(Arc::clone(&counts)),
            vec![StreamDelta::ToolCallReady(bash)],
        )
        .await;
        assert_eq!(counts.load(Ordering::SeqCst), 0);
        assert!(results.is_empty());
        assert!(sunk.is_empty());
    }

    #[tokio::test]
    async fn unregistered_speculative_tool_yields_error_outcome() {
        // The driver speculates on the tool CLASS: an offered call for a
        // speculative name that is not registered still "executes" — as an
        // error outcome the dispatch would surface.
        let engine = Engine {
            selector: Arc::new(StaticSelector::new("m", "mock")),
            providers: HashMap::new(),
            tools: Arc::new(ToolRegistry::new()),
            store: Arc::new(InMemoryStore::default()),
            config: EngineConfig::default(),
        };
        let (results, _) =
            run_driver(engine, vec![StreamDelta::ToolCallReady(glob_call("s1"))]).await;
        let outcome = results.get(&speculative_key(&glob_call("s1"))).unwrap();
        assert!(outcome.is_error);
        assert_eq!(outcome.content, "unknown tool");
    }

    #[test]
    fn allowed_for_gates_on_observers_and_denied_tools() {
        let clean = EngineConfig::default();
        assert!(allowed_for(&clean));

        // A denied speculative tool disarms the driver entirely.
        let mut denied_read = EngineConfig::default();
        denied_read.approvals.deny = vec!["read".into()];
        assert!(!allowed_for(&denied_read));

        // Denying a non-speculative tool keeps the driver armed.
        let mut denied_bash = EngineConfig::default();
        denied_bash.approvals.deny = vec!["bash".into()];
        assert!(allowed_for(&denied_bash));

        // Any observer (hooks, tool-event sink) keeps speculation off.
        let mut hooked = EngineConfig::default();
        hooked.hooks.pre_tool = Some("exit 0".into());
        assert!(!allowed_for(&hooked));
        let mut post = EngineConfig::default();
        post.hooks.post_tool = Some("exit 0".into());
        assert!(!allowed_for(&post));
        let observed = EngineConfig {
            on_tool_event: Some(Arc::new(|_activity| {})),
            ..EngineConfig::default()
        };
        assert!(!allowed_for(&observed));
    }
}

/// A panicked speculative task leaves the empty-error placeholder in the
/// map: dispatch treats it as a MISS and re-executes for real (cycle-3
/// adversarial F6/M6 pin). The placeholder value itself is asserted here.
#[test]
fn placeholder_is_recognizable_as_a_miss() {
    let placeholder = ToolOutcome::err(String::new());
    assert!(placeholder.is_error && placeholder.content.is_empty());
    // A real failure with content is NOT a miss (it is reused verbatim).
    let real_failure = ToolOutcome::err(String::from("grep exploded"));
    assert!(!(real_failure.is_error && real_failure.content.is_empty()));
}
