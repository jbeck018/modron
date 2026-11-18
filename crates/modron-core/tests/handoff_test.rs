//! C6c: a handed-off session takes no run, compaction or `!` command on the
//! host that handed it off; a longer log that continues past the HandedOff
//! line brings it back.

use modron_core::testing::{InMemoryStore, MockProvider, StaticSelector};
use modron_core::{Engine, EngineConfig, ModelProvider, SessionEvent, SessionStore, ToolRegistry};
use modron_types::Timestamp;
use std::collections::HashMap;
use std::sync::Arc;

fn engine(provider: Arc<dyn ModelProvider>, store: Arc<InMemoryStore>) -> Engine {
    let mut providers = HashMap::new();
    providers.insert("mock".to_string(), provider);
    Engine {
        selector: Arc::new(StaticSelector::new("mock-model", "mock")),
        providers,
        tools: Arc::new(ToolRegistry::new()),
        store,
        config: EngineConfig::default(),
    }
}

fn handed_off() -> SessionEvent {
    SessionEvent::HandedOff {
        to: Some("laptop".into()),
        at: Timestamp::now(),
        carrier_ref: "refs/heads/modron/handoff/x".into(),
    }
}

#[tokio::test]
async fn handed_off_session_refuses_runs_and_compaction_until_a_longer_log_arrives() {
    let store = Arc::new(InMemoryStore::new());
    let meta = store.create_session("handoff").await.unwrap();
    store
        .append(
            meta.id,
            &SessionEvent::Task {
                text: "first".into(),
            },
        )
        .await
        .unwrap();
    store.append(meta.id, &handed_off()).await.unwrap();

    let provider = Arc::new(MockProvider::new().then_text("done"));
    let engine = engine(provider.clone(), store.clone());

    let err = engine
        .run_task_resuming(meta.id, "next")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("handed off"), "{err}");
    assert!(
        err.contains("modron handoff import refs/heads/modron/handoff/x"),
        "{err}"
    );
    assert!(engine.compact_session(meta.id).await.is_err());
    assert_eq!(
        store.load(meta.id).await.unwrap().len(),
        2,
        "a refused run writes nothing"
    );

    // An import brings a log that continues past the HandedOff line.
    store
        .append(
            meta.id,
            &SessionEvent::Task {
                text: "from the other host".into(),
            },
        )
        .await
        .unwrap();
    let summary = engine
        .run_task_resuming(meta.id, "next")
        .await
        .expect("the longer log clears the handoff");
    assert_eq!(summary.session_id, meta.id);
}
