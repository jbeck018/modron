//! wake.rs - scheduled wakes for a session (H5a, protocol section 6).
//!
//! A wake is one prompt a session runs later: a one-shot (`after_minutes`)
//! or a recurring one (`every_minutes`). A session holds at most one wake;
//! setting a new one replaces the old. Each armed wake is a tokio timer
//! task that sleeps until due, then:
//! - idle session: starts a run with the prompt through the same starter the
//!   follow-up queue uses ([`SessionHub::start_queued`]);
//! - busy session: queues the prompt (`Enqueue` semantics); a full queue
//!   publishes a Note and skips this firing.
//!
//! A one-shot clears itself after firing; a recurring wake re-arms for
//! now + every. Every change publishes `WakeChanged`, and a firing also
//! publishes `Notify { kind: Wake }`.
//!
//! Persistence: every armed wake of this server goes into ONE JSON file next
//! to the automations registry (`<modron home>/automations/wakes-<key>.json`,
//! owner-only, written via temp file + rename). [`rearm_all`] reads it at
//! server start: a one-shot whose time passed while the server was down
//! fires once, at once; a recurring wake that missed its time re-arms for
//! now + every (missed recurring ticks are not replayed). A wake whose
//! session has no transcript any more is dropped.
//!
//! Time: every due time is read through a [`Clock`] (epoch seconds), so
//! tests inject a fake clock; production uses [`system_clock`].

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use modron_core::SessionId;
use modron_protocol::{NotifyKind, Payload, WakeWire};
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::dispatch::{ensure_session_for, ServerState};
use crate::hub::{notify_wake_text, QUEUE_FULL, STEER_IDLE};

/// Smallest one-shot delay, in minutes.
pub const MIN_AFTER_MINUTES: u32 = 1;
/// Smallest recurrence, in minutes.
pub const MIN_EVERY_MINUTES: u32 = 5;
/// Largest delay or recurrence (one week), in minutes.
pub const MAX_WAKE_MINUTES: u32 = 10_080;

/// Epoch seconds, injected so tests control when wakes are due.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// The production clock: wall-clock epoch seconds.
pub fn system_clock() -> Clock {
    Arc::new(crate::automations::now_epoch)
}

/// A validated wake schedule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Schedule {
    /// Fire once, `after_minutes` from now.
    Once { after_minutes: u32 },
    /// Fire every `every_minutes`, first after one period.
    Every { every_minutes: u32 },
}

impl Schedule {
    /// Seconds from now to the first firing.
    fn first_delay_secs(self) -> u64 {
        match self {
            Schedule::Once { after_minutes } => u64::from(after_minutes) * 60,
            Schedule::Every { every_minutes } => u64::from(every_minutes) * 60,
        }
    }

    /// The recurrence, or `None` for a one-shot.
    fn every(self) -> Option<u32> {
        match self {
            Schedule::Once { .. } => None,
            Schedule::Every { every_minutes } => Some(every_minutes),
        }
    }
}

/// Check the arguments of a `Wake` frame. Exactly one of `after` / `every`
/// must be set, each within its range, and the prompt must not be blank.
pub fn validate(
    after_minutes: Option<u32>,
    every_minutes: Option<u32>,
    prompt: &str,
) -> Result<Schedule, String> {
    if prompt.trim().is_empty() {
        return Err("wake prompt is empty".to_string());
    }
    match (after_minutes, every_minutes) {
        (Some(_), Some(_)) | (None, None) => {
            Err("set exactly one of after_minutes or every_minutes".to_string())
        }
        (Some(after), None) => {
            if (MIN_AFTER_MINUTES..=MAX_WAKE_MINUTES).contains(&after) {
                Ok(Schedule::Once {
                    after_minutes: after,
                })
            } else {
                Err(format!(
                    "after_minutes must be {MIN_AFTER_MINUTES}..={MAX_WAKE_MINUTES} (got {after})"
                ))
            }
        }
        (None, Some(every)) => {
            if (MIN_EVERY_MINUTES..=MAX_WAKE_MINUTES).contains(&every) {
                Ok(Schedule::Every {
                    every_minutes: every,
                })
            } else {
                Err(format!(
                    "every_minutes must be {MIN_EVERY_MINUTES}..={MAX_WAKE_MINUTES} (got {every})"
                ))
            }
        }
    }
}

/// One armed wake as a session handle holds it: the wire view, the token
/// naming this arming (a stale timer compares against it), and the timer.
pub struct ArmedWake {
    pub(crate) wire: WakeWire,
    pub(crate) token: u64,
    pub(crate) task: JoinHandle<()>,
}

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

fn next_token() -> u64 {
    NEXT_TOKEN.fetch_add(1, Ordering::Relaxed)
}

/// Whether the session has a transcript (a wake needs one: a firing resumes
/// the session).
async fn has_transcript(state: &ServerState, session: SessionId) -> bool {
    matches!(state.store.event_count(session).await, Ok(count) if count > 0)
}

/// Arm a wake for a session with a transcript, replacing any armed wake.
pub async fn set(
    state: &Arc<ServerState>,
    session: SessionId,
    schedule: Schedule,
    prompt: String,
    clock: Clock,
) -> Result<(), String> {
    if !has_transcript(state, session).await {
        return Err(format!("no such session {session}"));
    }
    let wire = WakeWire {
        next_at_unix: clock().saturating_add(schedule.first_delay_secs()),
        every_minutes: schedule.every(),
        prompt,
    };
    arm(state, session, wire, clock);
    Ok(())
}

/// Cancel a session's armed wake. Errors when none is armed.
pub fn cancel(state: &Arc<ServerState>, session: SessionId) -> Result<(), String> {
    let cancelled = state
        .hub
        .handle(&session)
        .is_some_and(|handle| handle.cancel_wake());
    if !cancelled {
        return Err("no wake is armed in this session".to_string());
    }
    persist(state);
    Ok(())
}

/// Drop a session's wake while the session is being deleted (no effect
/// when none is armed).
pub fn forget(state: &Arc<ServerState>, session: SessionId) {
    if let Some(handle) = state.hub.handle(&session) {
        if handle.cancel_wake() {
            persist(state);
        }
    }
}

/// Install `wire` as the session's wake: a fresh token, a timer task, the
/// `WakeChanged` frame, and the registry rewrite.
fn arm(state: &Arc<ServerState>, session: SessionId, wire: WakeWire, clock: Clock) {
    let handle = ensure_session_for(state, session);
    let token = next_token();
    let weak = Arc::downgrade(state);
    handle.arm_wake(token, wire, move || {
        tokio::spawn(timer(weak, session, token, clock))
    });
    persist(state);
}

/// The timer of one armed wake. Sleeps until due (re-checking the clock
/// after every sleep), stops as soon as its token is no longer the armed
/// wake, fires once, then clears (one-shot) or re-arms (recurring).
async fn timer(state: Weak<ServerState>, session: SessionId, token: u64, clock: Clock) {
    loop {
        let Some(state) = state.upgrade() else {
            return;
        };
        let Some(handle) = state.hub.handle(&session) else {
            return;
        };
        let Some(wire) = handle.wake_if_current(token) else {
            return;
        };
        let now = clock();
        if wire.next_at_unix > now {
            let delay = wire.next_at_unix - now;
            drop((state, handle));
            tokio::time::sleep(Duration::from_secs(delay)).await;
            continue;
        }
        // Settle before the prompt runs: a replaced or cancelled wake
        // returns false here and never fires.
        let next = wire.every_minutes.map(|every| WakeWire {
            next_at_unix: clock().saturating_add(u64::from(every) * 60),
            ..wire.clone()
        });
        let recurring = next.is_some();
        if !handle.settle_wake(token, next) {
            return;
        }
        fire(&state, session, &wire.prompt);
        persist(&state);
        if !recurring {
            return;
        }
        drop((state, handle));
    }
}

/// One firing: Notify, then start the prompt on an idle session or queue it
/// on a busy one. A full queue skips this firing with a Note.
fn fire(state: &ServerState, session: SessionId, prompt: &str) {
    let Some(handle) = state.hub.handle(&session) else {
        return;
    };
    handle.publish(Payload::Notify {
        kind: NotifyKind::Wake,
        text: notify_wake_text(prompt),
    });
    match state.hub.enqueue(session, prompt.to_string()) {
        Ok(()) => {}
        Err(message) if message == STEER_IDLE => {
            state.hub.start_queued(session, prompt.to_string());
        }
        Err(message) if message == QUEUE_FULL => handle.publish(Payload::Note {
            text: "wake skipped: queue full".to_string(),
        }),
        Err(message) => handle.publish(Payload::Note {
            text: format!("wake not started: {message}"),
        }),
    }
}

/// One wake as it sits in the registry file.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredWake {
    session_id: Uuid,
    #[serde(flatten)]
    wire: WakeWire,
}

/// The registry file: next to the automations registry, same directory.
fn registry_path(state: &ServerState) -> anyhow::Result<PathBuf> {
    let automations = crate::automations::registry_path(state)?;
    let dir = automations
        .parent()
        .ok_or_else(|| anyhow::anyhow!("automations registry has no directory"))?;
    Ok(dir.join(format!(
        "wakes-{}.json",
        crate::automations::working_dir_key(&state.working_dir)
    )))
}

fn load(state: &ServerState) -> Vec<StoredWake> {
    let Ok(path) = registry_path(state) else {
        return Vec::new();
    };
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            tracing::warn!(path = %path.display(), %error, "unreadable wake registry ignored");
            Vec::new()
        }),
        Err(_) => Vec::new(),
    }
}

/// Rewrite the registry from the live armed wakes (atomic, owner-only).
fn persist(state: &ServerState) {
    let path = match registry_path(state) {
        Ok(path) => path,
        Err(error) => {
            tracing::warn!(%error, "wake registry path unavailable");
            return;
        }
    };
    let rows: Vec<StoredWake> = state
        .hub
        .armed_wakes()
        .into_iter()
        .map(|(session_id, wire)| StoredWake { session_id, wire })
        .collect();
    let result = serde_json::to_vec_pretty(&rows)
        .map_err(anyhow::Error::from)
        .and_then(|bytes| crate::automations::write_private_atomic(&path, &bytes));
    if let Err(error) = result {
        tracing::warn!(path = %path.display(), %error, "wake registry write failed");
    }
}

/// Re-arm the registry's wakes at server start (see the module docs for the
/// missed-firing rules). Wakes whose session has no transcript are dropped.
pub async fn rearm_all(state: &Arc<ServerState>, clock: Clock) {
    let now = clock();
    for stored in load(state) {
        let session = stored.session_id;
        if !has_transcript(state, session).await {
            tracing::warn!(session = %session, "dropping wake: session has no transcript");
            continue;
        }
        let mut wire = stored.wire;
        // A one-shot already past due keeps its stamp: the timer sees it due
        // and fires once, at once. A recurring wake that missed its time
        // re-arms one period from now instead of replaying the missed ticks.
        if let Some(every) = wire.every_minutes {
            if wire.next_at_unix <= now {
                wire.next_at_unix = now.saturating_add(u64::from(every) * 60);
            }
        }
        arm(state, session, wire, clock.clone());
    }
    persist(state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::{BroadcastingStore, LiveFrame, SessionHub};
    use modron_core::testing::{InMemoryStore, StaticSelector};
    use modron_core::{Engine, EngineConfig, RunSummary, SessionEvent, SessionStore, ToolRegistry};
    use std::sync::Mutex as StdMutex;
    use tokio::sync::{broadcast, mpsc};

    /// A served state over an on-disk store rooted at `root`. Two states
    /// built from the same root share the store and the registry (a
    /// simulated restart).
    fn test_state(root: &std::path::Path) -> Arc<ServerState> {
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let hub = SessionHub::empty();
        hub.set_engine(Engine {
            selector: Arc::new(StaticSelector::new("mock-model", "mock")),
            providers: Default::default(),
            tools: Arc::new(ToolRegistry::new()),
            store: Arc::new(InMemoryStore::new()),
            config: EngineConfig {
                default_model: "mock-model".into(),
                default_provider: "mock".into(),
                ..EngineConfig::default()
            },
        });
        let inner: Arc<dyn SessionStore> =
            Arc::new(modron_store::JsonlSessionStore::new(root.join("sessions")));
        let store: Arc<dyn SessionStore> = Arc::new(BroadcastingStore::new(inner, hub.clone()));
        Arc::new(ServerState {
            hub,
            store,
            devices_dir: root.join("home").join("clients"),
            version: "test".into(),
            working_dir: work,
            tasks: crate::tasks::TaskRegistry::new(),
            bridge_base: None,
            rc_keys: StdMutex::new(Default::default()),
            rc_tunnels: StdMutex::new(Default::default()),
        })
    }

    /// A session with one transcript event, so it can be woken.
    async fn live_session(state: &Arc<ServerState>) -> SessionId {
        let id = state.store.create_session("wake test").await.unwrap().id;
        state
            .store
            .append(
                id,
                &SessionEvent::Task {
                    text: "earlier work".into(),
                },
            )
            .await
            .unwrap();
        id
    }

    /// A clock the test moves by hand.
    fn fake_clock(start: u64) -> (Arc<std::sync::atomic::AtomicU64>, Clock) {
        let now = Arc::new(std::sync::atomic::AtomicU64::new(start));
        let reader = Arc::clone(&now);
        (now, Arc::new(move || reader.load(Ordering::SeqCst)))
    }

    /// Starter that forwards every call to a channel the test reads.
    fn recording_starter(state: &ServerState) -> mpsc::UnboundedReceiver<(SessionId, String)> {
        let (tx, rx) = mpsc::unbounded_channel();
        state.hub.set_run_starter(Arc::new(move |id, text| {
            let _ = tx.send((id, text));
        }));
        rx
    }

    /// A run that parks until the test interrupts it.
    fn parked_run(id: SessionId) -> crate::hub::RunFuture {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(RunSummary {
                session_id: id,
                answer: "unreachable".into(),
                turns: 0,
                tool_calls: 0,
                usage: Default::default(),
                models_used: vec![],
            })
        })
    }

    async fn next_of(rx: &mut broadcast::Receiver<LiveFrame>, kind: &str) -> serde_json::Value {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let frame = rx.recv().await.expect("live frame");
                let json: serde_json::Value = serde_json::from_str(&frame.json).unwrap();
                if json["type"] == kind {
                    return json;
                }
            }
        })
        .await
        .expect("frame arrives")
    }

    async fn recv_start(
        rx: &mut mpsc::UnboundedReceiver<(SessionId, String)>,
    ) -> (SessionId, String) {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("starter called")
            .expect("starter channel open")
    }

    #[test]
    fn validate_accepts_one_shot_and_recurring_and_rejects_bad_arguments() {
        assert_eq!(
            validate(Some(1), None, "later"),
            Ok(Schedule::Once { after_minutes: 1 })
        );
        assert_eq!(
            validate(None, Some(5), "poll"),
            Ok(Schedule::Every { every_minutes: 5 })
        );
        assert_eq!(
            validate(Some(MAX_WAKE_MINUTES), None, "x"),
            Ok(Schedule::Once {
                after_minutes: MAX_WAKE_MINUTES
            })
        );
        assert!(validate(Some(0), None, "x").is_err());
        assert!(validate(Some(MAX_WAKE_MINUTES + 1), None, "x").is_err());
        assert!(validate(None, Some(4), "x").is_err());
        assert!(validate(None, Some(MAX_WAKE_MINUTES + 1), "x").is_err());
        assert!(validate(None, None, "x").is_err());
        assert!(validate(Some(5), Some(5), "x").is_err());
        assert!(validate(Some(5), None, "   ").is_err());
        assert_eq!(
            validate(Some(5), None, "   ").unwrap_err(),
            "wake prompt is empty"
        );
    }

    #[tokio::test]
    async fn set_replace_and_cancel_publish_wake_changed_and_show_in_live_state() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        let id = live_session(&state).await;
        let (_clock, clock) = fake_clock(1_000);
        let handle = ensure_session_for(&state, id);
        let mut rx = handle.subscribe();

        set(
            &state,
            id,
            Schedule::Once { after_minutes: 5 },
            "first".into(),
            clock.clone(),
        )
        .await
        .unwrap();
        let changed = next_of(&mut rx, "wake_changed").await;
        assert_eq!(changed["wake"]["next_at_unix"], 1_300);
        assert_eq!(
            handle.live_state().wake,
            Some(WakeWire {
                next_at_unix: 1_300,
                every_minutes: None,
                prompt: "first".into(),
            })
        );

        // A new wake replaces the old one.
        set(
            &state,
            id,
            Schedule::Every { every_minutes: 10 },
            "second".into(),
            clock.clone(),
        )
        .await
        .unwrap();
        let changed = next_of(&mut rx, "wake_changed").await;
        assert_eq!(changed["wake"]["every_minutes"], 10);
        let live = handle.live_state().wake.expect("armed");
        assert_eq!(live.prompt, "second");
        assert_eq!(live.every_minutes, Some(10));

        cancel(&state, id).unwrap();
        let changed = next_of(&mut rx, "wake_changed").await;
        assert!(changed["wake"].is_null());
        assert!(handle.live_state().wake.is_none());
        assert!(cancel(&state, id).is_err(), "nothing left to cancel");
    }

    #[tokio::test]
    async fn wake_on_a_session_without_a_transcript_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        let (_clock, clock) = fake_clock(0);
        let err = set(
            &state,
            SessionId::new_v4(),
            Schedule::Once { after_minutes: 1 },
            "x".into(),
            clock,
        )
        .await
        .unwrap_err();
        assert!(err.starts_with("no such session"));
    }

    #[tokio::test]
    async fn due_one_shot_on_an_idle_session_starts_the_prompt_and_clears() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        let id = live_session(&state).await;
        let mut starts = recording_starter(&state);
        let (_clock, clock) = fake_clock(1_000);
        let handle = ensure_session_for(&state, id);
        let mut rx = handle.subscribe();

        // Already due: the timer fires at once.
        arm(
            &state,
            id,
            WakeWire {
                next_at_unix: 1_000,
                every_minutes: None,
                prompt: "check the deploy".into(),
            },
            clock,
        );
        assert_eq!(
            recv_start(&mut starts).await,
            (id, "check the deploy".to_string())
        );
        assert!(handle.live_state().wake.is_none(), "one-shot clears");
        let notify = next_of(&mut rx, "notify").await;
        assert_eq!(notify["kind"], "wake");
        assert_eq!(notify["text"], "wake: check the deploy");
    }

    #[tokio::test]
    async fn due_wake_on_a_busy_session_enqueues_the_prompt() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        let id = live_session(&state).await;
        let mut starts = recording_starter(&state);
        let handle = ensure_session_for(&state, id);
        state.hub.start_run(id, parked_run(id)).unwrap();
        let (_clock, clock) = fake_clock(1_000);

        arm(
            &state,
            id,
            WakeWire {
                next_at_unix: 1_000,
                every_minutes: None,
                prompt: "after the build".into(),
            },
            clock,
        );
        for _ in 0..200 {
            if handle
                .live_state()
                .queued
                .contains(&"after the build".to_string())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            handle.live_state().queued,
            vec!["after the build".to_string()]
        );
        assert!(handle.live_state().wake.is_none());
        assert!(starts.try_recv().is_err(), "a busy session starts nothing");
        handle.interrupt();
    }

    #[tokio::test]
    async fn recurring_wake_rearms_for_one_period_after_each_firing() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        let id = live_session(&state).await;
        let mut starts = recording_starter(&state);
        let handle = ensure_session_for(&state, id);
        let (_clock, clock) = fake_clock(1_000);

        arm(
            &state,
            id,
            WakeWire {
                next_at_unix: 1_000,
                every_minutes: Some(5),
                prompt: "poll".into(),
            },
            clock,
        );
        assert_eq!(recv_start(&mut starts).await, (id, "poll".to_string()));
        // The firing settles (re-arm) before the prompt starts; wait for it.
        for _ in 0..200 {
            if handle
                .live_state()
                .wake
                .is_some_and(|w| w.next_at_unix == 1_300)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let wake = handle.live_state().wake.expect("re-armed");
        assert_eq!(wake.next_at_unix, 1_300, "now + every_minutes");
        assert_eq!(wake.every_minutes, Some(5));
        cancel(&state, id).unwrap();
    }

    #[tokio::test]
    async fn persisted_wakes_rearm_after_a_restart_and_overdue_one_shots_fire_once() {
        let root = tempfile::tempdir().unwrap();
        let first = test_state(root.path());
        let later = live_session(&first).await;
        let overdue = live_session(&first).await;
        let (_clock, clock) = fake_clock(1_000);
        // Both armed in the future so neither fires inside the first server;
        // the overdue one is backdated on disk below, as if the server had
        // been down while it came due.
        for (session, prompt) in [(later, "tomorrow"), (overdue, "missed while down")] {
            arm(
                &first,
                session,
                WakeWire {
                    next_at_unix: 9_000,
                    every_minutes: None,
                    prompt: prompt.into(),
                },
                clock.clone(),
            );
        }
        let path = registry_path(&first).unwrap();
        let mut rows: Vec<serde_json::Value> =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        for row in &mut rows {
            if row["session_id"] == serde_json::json!(overdue) {
                row["next_at_unix"] = serde_json::json!(900);
            }
        }
        std::fs::write(&path, serde_json::to_vec_pretty(&rows).unwrap()).unwrap();
        // Simulated restart: a fresh hub over the same store and registry.
        drop(first);
        let second = test_state(root.path());
        let mut starts = recording_starter(&second);
        rearm_all(&second, clock).await;

        // The overdue one-shot fires once, at once.
        assert_eq!(
            recv_start(&mut starts).await,
            (overdue, "missed while down".to_string())
        );
        // The future wake is re-armed with its stamp intact.
        let handle = second.hub.handle(&later).expect("re-armed session is live");
        assert_eq!(
            handle.live_state().wake,
            Some(WakeWire {
                next_at_unix: 9_000,
                every_minutes: None,
                prompt: "tomorrow".into(),
            })
        );
        cancel(&second, later).unwrap();
    }

    #[tokio::test]
    async fn forget_drops_the_wake_and_the_registry_entry() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        let id = live_session(&state).await;
        let (_clock, clock) = fake_clock(1_000);
        set(
            &state,
            id,
            Schedule::Once { after_minutes: 30 },
            "gone".into(),
            clock,
        )
        .await
        .unwrap();
        assert!(load(&state).iter().any(|row| row.session_id == id));
        forget(&state, id);
        assert!(state.hub.handle(&id).unwrap().live_state().wake.is_none());
        assert!(load(&state).is_empty());
    }
}
