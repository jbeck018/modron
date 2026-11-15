//! SessionHub — the remote-control session server core (Phase 2).
//!
//! Every live session sits behind a [`SessionHandle`]: one broadcast
//! channel (capacity 1024) that N attached clients subscribe to, a
//! per-session monotonic `seq` for stream ordering, the session's own
//! [`Engine`] clone carrying the hub's sinks, and the approval/interrupt
//! state. Engines need zero changes to reach the wire: they already append
//! every event to their store, and the store here is a
//! [`BroadcastingStore`] decorator that persists, then notifies the hub.
//!
//! Frame ordering: [`SessionHandle::publish`] assigns `seq` and sends under
//! one mutex, so frames from any task (engine sinks, approval resolutions)
//! are strictly ordered per session.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use crate::wake::ArmedWake;
use async_trait::async_trait;
use modron_core::permissions::{Rule, RuleEffect, RuleSet};
use modron_core::{
    ApprovalAsk, ContextGauge, Engine, Phase, PlanItem, RunSummary, SessionEvent, SessionId,
    SessionMeta, SessionStore, SteerInbox, StreamDelta, ToolActivity, ToolCall, Usage,
    UserShellOutcome,
};
use modron_protocol::{
    ApprovalModeWire, ConfigPatch, ContextGaugeWire, Decision, Frame, LiveState, NotifyKind,
    Payload, PendingApproval, PlanDecision, RuleWire, WakeWire, WireDelta, WireRunSummary,
    WireSwarmRow, WireToolActivity,
};
use modron_view::ActivityTracker;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use uuid::Uuid;

/// Broadcast-channel capacity per session (frames, not bytes). A client
/// slower than 1024 queued frames is lagged and must re-attach.
const CHANNEL_CAPACITY: usize = 1024;

/// Maximum messages held in a session's follow-up queue (`Enqueue`).
pub const QUEUE_CAP: usize = 20;

/// Error text for an enqueue that would exceed [`QUEUE_CAP`].
pub const QUEUE_FULL: &str = "queue full";

/// Error text for a plan decision when no plan is awaiting one (H2b).
pub const NO_PLAN_PENDING: &str = "no plan is pending";

/// Starts a queued message as a new run for the session. Installed once by
/// the dispatch layer (it owns the run-building path). The hub calls it
/// only after every hub lock is released, from the completed run's task.
pub type RunStarter = Arc<dyn Fn(SessionId, String) + Send + Sync>;

/// Lock a mutex, recovering from poisoning. The hub's data stays usable
/// after a panic elsewhere — same convention as `modron_core::testing`.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------
// BroadcastingStore — persisted events reach the wire with zero engine changes
// ---------------------------------------------------------------------------

/// [`SessionStore`] decorator: persists to the inner store, then notifies
/// the hub so attached clients receive a `Payload::Event` frame for every
/// persisted event. The engine's own `store` is this decorator, so its
/// appends fan out untouched.
pub struct BroadcastingStore {
    inner: Arc<dyn SessionStore>,
    hub: SessionHub,
    /// Per-session append serialization: the JSONL store's
    /// write-then-rename meta update is not safe under concurrent appends
    /// for one session, and the abort window of a run can overlap the
    /// next run's first writes. `create_session` shares the same lock so
    /// transcript creation cannot interleave with an append.
    locks: tokio::sync::Mutex<HashMap<SessionId, Arc<tokio::sync::Mutex<()>>>>,
}

impl BroadcastingStore {
    pub fn new(inner: Arc<dyn SessionStore>, hub: SessionHub) -> Self {
        Self {
            inner,
            hub,
            locks: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// The wrapped store (sessions on disk, without the hub hop).
    pub fn inner(&self) -> &Arc<dyn SessionStore> {
        &self.inner
    }

    /// The per-session write lock (the lock map itself is tiny and
    /// uncontended; clone the Arc and release the map before awaiting).
    async fn write_lock(&self, id: SessionId) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.locks.lock().await;
        locks.entry(id).or_default().clone()
    }
}

#[async_trait]
impl SessionStore for BroadcastingStore {
    async fn create_session(&self, title: &str) -> modron_core::Result<SessionMeta> {
        // Creation interleaves with appends only through the run adoption
        // path; serialize under the (not-yet-known) session's lock is
        // impossible pre-id, and the append side takes its own lock — the
        // inner store's create is atomic per file, so no extra guard.
        self.inner.create_session(title).await
    }

    async fn append(&self, id: SessionId, event: &SessionEvent) -> modron_core::Result<()> {
        let lock = self.write_lock(id).await;
        let _guard = lock.lock().await;
        self.inner.append(id, event).await?;
        self.hub.on_persisted(id, event);
        Ok(())
    }

    async fn load(&self, id: SessionId) -> modron_core::Result<Vec<SessionEvent>> {
        self.inner.load(id).await
    }

    /// Hold the per-session APPEND lock across the read: attach's
    /// watermark read is then exact (an append blocked on this lock can
    /// neither write to disk nor publish) - the cycle-2 F1 fix.
    async fn load_snapshot_locked(
        &self,
        id: SessionId,
    ) -> modron_core::Result<(tokio::sync::OwnedMutexGuard<()>, Vec<SessionEvent>)> {
        let lock = self.write_lock(id).await;
        let guard = lock.clone().lock_owned().await;
        let events = self.inner.load(id).await?;
        Ok((guard, events))
    }

    async fn list(&self) -> modron_core::Result<Vec<SessionMeta>> {
        self.inner.list().await
    }

    async fn delete(&self, id: SessionId) -> modron_core::Result<bool> {
        // Serialize the delete with appends, then drop the per-session lock
        // entry: the map must not keep one lock per deleted session forever.
        let lock = self.write_lock(id).await;
        let _guard = lock.lock().await;
        let removed = self.inner.delete(id).await?;
        self.locks.lock().await.remove(&id);
        Ok(removed)
    }

    async fn event_count(&self, id: SessionId) -> modron_core::Result<u64> {
        self.inner.event_count(id).await
    }

    /// Fork = create the new transcript, then copy events into it. The
    /// trait's default (append without create) cannot work against
    /// file-backed stores, which require `create_session` first. Copied
    /// events go through `inner` directly — attached clients of the fork
    /// get the full log via their attach Snapshot instead of a replay.
    async fn fork(&self, from: SessionId) -> modron_core::Result<SessionId> {
        let events = self.inner.load(from).await?;
        let meta = self
            .inner
            .create_session(&format!("fork of {from}"))
            .await?;
        // One batched write: per-event appends meant one meta rewrite +
        // several syscalls PER EVENT (a 5k-event fork churned the disk for
        // seconds; cycle-2 audit H5).
        self.inner.append_batch(meta.id, &events).await?;
        Ok(meta.id)
    }
}

/// Per-run store: adopts a pre-created session. `Submit` on the server
/// reserves the session id synchronously (so the hub handle exists before
/// any frame flows) and the engine's own `create_session` — the first store
/// call of `run_task_with_attachments` — is answered with the reserved
/// session instead of opening a second one.
struct PinnedCreateStore {
    inner: Arc<dyn SessionStore>,
    pin: Mutex<Option<SessionId>>,
}

impl PinnedCreateStore {
    fn new(inner: Arc<dyn SessionStore>, pinned: Option<SessionId>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            pin: Mutex::new(pinned),
        })
    }
}

#[async_trait]
impl SessionStore for PinnedCreateStore {
    async fn create_session(&self, title: &str) -> modron_core::Result<SessionMeta> {
        let pinned = lock(&self.pin).take();
        match pinned {
            // Adoption: the caller pre-created this session; hand back its
            // existing meta instead of opening a second one. If the session
            // has vanished in the meantime, fall through to a fresh create.
            Some(id) => {
                // Existence probe via event_count - cheap after the store's
                // one-time seed (a full list() parsed every meta on disk
                // per adopted submit; cycle-2 audit H14).
                let exists = self.inner.event_count(id).await;
                match exists {
                    Ok(_) => self
                        .inner
                        .list()
                        .await?
                        .into_iter()
                        .find(|m| m.id == id)
                        .ok_or_else(|| {
                            modron_core::Error::Session(format!(
                                "pinned session {id} metadata unreadable"
                            ))
                        }),
                    // Adoption of a vanished session must fail LOUDLY: the
                    // old fallback minted a brand-new uuid the caller's
                    // sinks do not know - a run with zero live frames under
                    // a surprise id (cycle-2 audit L4).
                    Err(e) => Err(modron_core::Error::Session(format!(
                        "pinned session {id} vanished before adoption: {e}"
                    ))),
                }
            }
            None => self.inner.create_session(title).await,
        }
    }

    async fn append(&self, id: SessionId, event: &SessionEvent) -> modron_core::Result<()> {
        self.inner.append(id, event).await
    }

    async fn load(&self, id: SessionId) -> modron_core::Result<Vec<SessionEvent>> {
        self.inner.load(id).await
    }

    async fn list(&self) -> modron_core::Result<Vec<SessionMeta>> {
        self.inner.list().await
    }

    async fn event_count(&self, id: SessionId) -> modron_core::Result<u64> {
        self.inner.event_count(id).await
    }
}

// ---------------------------------------------------------------------------
// SessionHandle — one live session
// ---------------------------------------------------------------------------

/// The boxed engine future a `Submit` produces (which `Engine::run_task*`
/// variant to drive for this run).
pub type RunFuture = Pin<Box<dyn Future<Output = modron_core::Result<RunSummary>> + Send>>;

/// A user-issued `!command` run (PARITY-C), occupying the run slot like a
/// turn but ending with no Notify.
pub type ShellRunFuture =
    Pin<Box<dyn Future<Output = modron_core::Result<UserShellOutcome>> + Send>>;

/// How a run ended, as the terminal frame sees it. A turn may notify; a
/// shell run never does.
pub enum RunEnd {
    Turn(modron_core::Result<RunSummary>),
    Shell(modron_core::Result<UserShellOutcome>),
}

/// Error text for a steer sent while the session has no active run.
pub const STEER_IDLE: &str = "session idle - submit instead";

/// One fanned-out live frame: identity plus the JSON already serialized
/// once at publish (every attached connection sends the same bytes).
#[derive(Clone, Debug)]
pub struct LiveFrame {
    pub session: SessionId,
    pub seq: u64,
    pub json: std::sync::Arc<str>,
}

/// One live session: broadcast channel + seq counter + run state + the
/// per-session [`Engine`] clone carrying the hub's sinks.
///
/// Field locking discipline: every mutex is short-lived and never held
/// across an await. `seq` guards publish ordering; `run` doubles as the
/// busy flag AND the terminal-frame lock (see [`SessionHandle::complete`]).
///
/// Lock order (outermost first): `run` -> `gate_order` -> `seq` -> every
/// state mutex (`engine`, `tracker`, `text`, `reasoning`, `live_approval`,
/// `usage`, `pending`, labels). Every sink mutates its state and publishes
/// under `seq` in one scope, so a snapshot read under `seq` is consistent
/// with the watermark it reports (LG-d). Nothing takes `run` below `seq`.
pub struct SessionHandle {
    pub id: SessionId,
    tx: broadcast::Sender<LiveFrame>,
    seq: Mutex<u64>,
    /// The active run's task; `Some` = busy. Taken by whoever emits the
    /// terminal frame (interrupt on abort, the run task on completion).
    run: Mutex<Option<JoinHandle<()>>>,
    /// Set by [`SessionHandle::interrupt`](Self::interrupt) before the
    /// abort lands, so the completing run knows a TurnAborted was already
    /// published on its behalf.
    /// request_id -> decision channel; the engine parks on the receiver
    /// inside its approver (`block_in_place`, the TUI's pattern).
    pending: Mutex<HashMap<String, std::sync::mpsc::Sender<bool>>>,
    /// Orders "insert into pending + publish the request" against
    /// interrupt's "drain pending + publish resolutions + abort": a
    /// request frame can never land after TurnAborted (cycle-3 F3 - the
    /// cycle-2 fix narrowed this race but the drain could still land
    /// between the gate's insert and its publish).
    gate_order: Mutex<()>,
    /// Set by interrupt (under gate_order): gates that arrive afterwards
    /// decline silently instead of publishing into a dead run. Written only
    /// with the run slot held, so an interrupt and a new run's start cannot
    /// interleave their writes (LG-c).
    aborted: AtomicBool,
    /// Mirror of the run slot's occupancy, written under the slot lock, so
    /// [`live_state`](Self::live_state) reads it without taking the slot
    /// (the snapshot holds the seq lock and must not nest under the slot).
    busy: AtomicBool,
    /// Set by [`SessionHub::begin_delete`]: no run may start until the
    /// delete settles. Written and read under the run-slot lock.
    deleting: AtomicBool,
    /// The shared broadcasting store, kept so a submit can re-arm the
    /// adoption pin ([`rearm_adoption`](Self::rearm_adoption)).
    shared_store: Arc<dyn SessionStore>,
    /// Approval wait before an auto-deny (injectable for tests; prod = 30s).
    approval_timeout: Mutex<std::time::Duration>,
    /// Sticky approve-rest (the TUI's `a` answer): every later mutating
    /// call in this session auto-approves.
    allow_rest: AtomicBool,
    /// Canonical grouped tool view for Snapshot `live`.
    tracker: Mutex<ActivityTracker>,
    /// In-flight streaming buffers (cleared on the `Done` delta).
    text: Mutex<String>,
    reasoning: Mutex<String>,
    /// Approvals awaiting a decision, oldest first (H3b: concurrent child
    /// asks queue here). The Snapshot shows the front
    /// (`live.pending_approval`).
    live_approval: Mutex<VecDeque<PendingApproval>>,
    /// Cumulative provider-reported usage (folded from Usage events).
    usage: Mutex<Usage>,
    /// Status-line labels.
    model: Mutex<String>,
    provider: Mutex<String>,
    goal: Mutex<String>,
    /// This session's engine clone (sinks + adoption store wired in
    /// [`SessionHub::ensure_session`]). Mutex-guarded so a concurrent
    /// SetConfig can never interleave with a Submit cloning it.
    engine: Mutex<Engine>,
    /// Steer messages waiting for the run loop (the same inbox as
    /// `engine.config.steer`). Created with the handle; never replaced.
    steer: Arc<SteerInbox>,
    /// Follow-up messages (`Enqueue`), front = next to run. Held across
    /// interrupts; popped only when a run completes successfully. Lock
    /// order: `run` -> `seq` -> `queue`.
    queue: Mutex<VecDeque<String>>,
    /// The newest `SessionEvent::Plan` this handle has seen via
    /// `on_persisted` (H2b). Read when a plan-phase run completes.
    plan: Mutex<Vec<PlanItem>>,
    /// A proposed plan awaits a decision (H2b). Set when a plan-phase run
    /// completes with a plan; cleared only by `decide_plan`.
    plan_pending: AtomicBool,
    /// The last context gauge computed for this session (H9b). `None`
    /// until a run or a `Compact` on this handle computes one. Written
    /// under the seq lock together with its `ContextChanged` frame.
    context: Mutex<Option<ContextGauge>>,
    /// The armed scheduled wake (H5a), if any. Written under the seq lock
    /// together with its `WakeChanged` frame (see `arm_wake`).
    wake: Mutex<Option<ArmedWake>>,
}

impl SessionHandle {
    /// Locked access to the session's engine.
    pub fn engine_cell(&self) -> MutexGuard<'_, Engine> {
        lock(&self.engine)
    }

    /// Re-arm the adoption pin so the next engine `create_session` on this
    /// handle adopts this session. The pin is single-use: after a first run
    /// that wrote no event, a later submit on the still-empty session would
    /// otherwise open a second session id with no live handle (LG-e).
    /// Called before each submit to a known session; clones already taken
    /// by running engines keep their own store.
    pub fn rearm_adoption(&self) {
        let mut engine = self.engine_cell();
        engine.store = PinnedCreateStore::new(Arc::clone(&self.shared_store), Some(self.id));
    }

    /// Assign the next seq and broadcast. The seq lock spans the send so
    /// concurrent publishers cannot reorder frames.
    pub fn publish(&self, payload: Payload) {
        let mut seq = lock(&self.seq);
        self.publish_locked(&mut seq, payload);
    }

    /// Publish while the caller holds the seq lock (and has already applied
    /// the state change the frame reports, under that same lock).
    fn publish_locked(&self, seq: &mut u64, payload: Payload) {
        *seq += 1;
        // Serialize ONCE for all viewers (2026-11-12 audit: the broadcast
        // used to deep-clone the Frame per subscriber and every connection
        // re-serialized it; large ToolResult events multiplied by viewer
        // count). No viewers = no serialization either.
        if self.tx.receiver_count() > 0 {
            let frame = Frame::event(Some(self.id), *seq, payload);
            if let Ok(json) = serde_json::to_string(&frame) {
                let _ = self.tx.send(LiveFrame {
                    session: self.id,
                    seq: *seq,
                    json: std::sync::Arc::from(json.as_str()),
                });
            }
        }
    }

    /// The seq watermark a Snapshot should carry.
    pub fn current_seq(&self) -> u64 {
        *lock(&self.seq)
    }

    /// The seq watermark and the live state, read under ONE seq-lock scope.
    /// Every sink applies its state change and publishes under the seq lock,
    /// so the live state here includes exactly the frames at or below the
    /// returned watermark: a client that applies the snapshot and then only
    /// frames with `seq > watermark` neither loses nor repeats a delta.
    pub fn snapshot_point(&self) -> (u64, LiveState) {
        let seq = lock(&self.seq);
        let watermark = *seq;
        let live = self.live_state();
        (watermark, live)
    }

    /// Subscribe to this session's frame stream (attach step 1: subscribe
    /// BEFORE loading the snapshot so no live frame is missed).
    pub fn subscribe(&self) -> broadcast::Receiver<LiveFrame> {
        self.tx.subscribe()
    }

    /// Busy = a run task is parked in the slot.
    pub fn is_busy(&self) -> bool {
        lock(&self.run).is_some()
    }

    /// The working dir this session's engine runs in (worktree tasks run
    /// inside their worktree).
    pub fn working_dir(&self) -> PathBuf {
        lock(&self.engine).config.working_dir.clone()
    }

    /// The live (non-persisted) half of a snapshot.
    pub fn live_state(&self) -> LiveState {
        // The effective config is the source of truth for the mode,
        // sandbox, phase and allow/deny lists (apply_config writes it, the
        // session's engine clone carries it). Copy them out and drop the
        // engine lock before taking the tracker lock.
        let (approval_mode, sandbox, allow, deny, rules, phase, effort) = {
            let engine = self.engine_cell();
            let config = &engine.config;
            let mode = match config.approvals.mode {
                modron_core::ApprovalMode::Deny => ApprovalModeWire::Deny,
                modron_core::ApprovalMode::Ask => ApprovalModeWire::Ask,
                modron_core::ApprovalMode::ApproveAll => ApprovalModeWire::ApproveAll,
            };
            (
                Some(mode),
                Some(config.sandbox),
                config.approvals.allow.clone(),
                config.approvals.deny.clone(),
                config
                    .approvals
                    .rules
                    .rules()
                    .iter()
                    .map(rule_wire)
                    .collect::<Vec<_>>(),
                Some(config.phase),
                config.effort,
            )
        };
        let tracker = lock(&self.tracker);
        LiveState {
            approval_mode,
            sandbox,
            allow,
            deny,
            rules,
            phase,
            effort,
            roles: self.engine_cell().config.roles.clone(),
            plan_pending: self.plan_pending.load(Ordering::Relaxed),
            running: self.busy.load(Ordering::Relaxed),
            streaming_text: lock(&self.text).clone(),
            streaming_reasoning: lock(&self.reasoning).clone(),
            turn_tools: tracker.turn_tools.clone(),
            subagents: tracker
                .subagents
                .iter()
                .map(|row| WireSwarmRow {
                    id: row.id.clone(),
                    label: row.label.clone(),
                    turns: row.turns,
                    tokens: row.tokens,
                    last_tool: row.last_tool.clone(),
                    done: row.done,
                    state: row.state,
                })
                .collect(),
            pending_approval: lock(&self.live_approval).front().cloned(),
            cumulative_usage: *lock(&self.usage),
            model: lock(&self.model).clone(),
            provider: lock(&self.provider).clone(),
            goal: lock(&self.goal).clone(),
            queued: lock(&self.queue).iter().cloned().collect(),
            context: lock(&self.context).map(|gauge| ContextGaugeWire {
                used: gauge.used,
                window: gauge.window,
            }),
            wake: self.wake_wire(),
        }
    }

    /// Delta sink: convert to the wire form, buffer the streaming state,
    /// clear buffers on `Done` (the per-turn boundary), publish.
    pub fn on_stream_delta(&self, delta: StreamDelta) {
        // Buffer mutation and publish share one seq scope (see snapshot_point).
        match WireDelta::from_core(&delta) {
            Some(WireDelta::Text { text }) => {
                let mut seq = lock(&self.seq);
                lock(&self.text).push_str(&text);
                self.publish_locked(
                    &mut seq,
                    Payload::Delta {
                        delta: WireDelta::Text { text },
                    },
                );
            }
            Some(WireDelta::Reasoning { text }) => {
                let mut seq = lock(&self.seq);
                lock(&self.reasoning).push_str(&text);
                self.publish_locked(
                    &mut seq,
                    Payload::Delta {
                        delta: WireDelta::Reasoning { text },
                    },
                );
            }
            Some(WireDelta::Done) => {
                let mut seq = lock(&self.seq);
                *lock(&self.text) = String::new();
                *lock(&self.reasoning) = String::new();
                lock(&self.tracker).clear_turn();
                self.publish_locked(
                    &mut seq,
                    Payload::Delta {
                        delta: WireDelta::Done,
                    },
                );
            }
            // `ToolCallReady` never reaches sinks (engine intercepts it).
            None => {}
        }
    }

    /// Tool-activity sink: feed the session's tracker, publish.
    pub fn on_tool_activity(&self, activity: ToolActivity) {
        let mut seq = lock(&self.seq);
        lock(&self.tracker).on_tool(&activity);
        // H3d: a background child's settlement is pushed (AgentFinished plus
        // its Notify) instead of being a swarm-row change.
        if let ToolActivity::BackgroundFinished {
            id,
            label,
            ok,
            brief,
        } = &activity
        {
            self.publish_locked(
                &mut seq,
                Payload::AgentFinished {
                    agent: id.clone(),
                    ok: *ok,
                    brief: brief.clone(),
                },
            );
            self.publish_locked(
                &mut seq,
                Payload::Notify {
                    kind: if *ok {
                        NotifyKind::Done
                    } else {
                        NotifyKind::Failed
                    },
                    text: notify_agent_text(label, *ok),
                },
            );
            return;
        }
        self.publish_locked(
            &mut seq,
            Payload::Tool {
                activity: WireToolActivity::from(&activity),
            },
        );
    }

    /// The session approver (Ask mode). Sticky allow-rest short-circuits;
    /// otherwise register the request, broadcast it to every attached
    /// client, and park the engine task on a sync channel until a client
    /// answers (or an interrupt drains the pending map with denials, or
    /// the approval timeout elapses (30s in production) — an
    /// unanswered request can never wedge the session).
    /// Override the approval wait (tests: prove the timeout path without
    /// a 30s sleep).
    pub fn set_approval_timeout(&self, timeout: std::time::Duration) {
        *lock(&self.approval_timeout) = timeout;
    }

    fn approval_timeout(&self) -> std::time::Duration {
        *lock(&self.approval_timeout)
    }

    /// `agent` is `Some(label)` for a delegated child's ask (H3b). A child
    /// ask is honoured only while the run is live: a background child that
    /// outlives its parent run is refused, never parked. Top-level asks come
    /// from the run itself, so they skip that check.
    pub fn approve_gate(&self, call: &ToolCall, agent: Option<&str>) -> bool {
        if agent.is_none() && self.allow_rest.load(Ordering::Relaxed) {
            return true;
        }
        let approval_timeout = self.approval_timeout();
        let request_id = format!("req-{}", Uuid::new_v4());
        let (respond, receive) = std::sync::mpsc::channel::<bool>();
        // The insert + publish pair is atomic against interrupt(): if the
        // run was already aborted (slot taken), the request is never
        // published at all; otherwise any interrupt drain is ordered
        // AFTER this publish, so the client sees request -> resolution ->
        // abort, never request-after-abort (cycle-3 F3).
        let order_guard = lock(&self.gate_order);
        if self.aborted.load(Ordering::Relaxed) {
            // Interrupt already published its resolutions + abort: a late
            // gate must not re-open the strip (cycle-3 F3).
            return false;
        }
        if agent.is_some() && !self.busy.load(Ordering::Relaxed) {
            // The run ended (complete() clears `busy` under gate_order):
            // no approver may outlive its run.
            return false;
        }
        if self.allow_rest.load(Ordering::Relaxed) {
            // Sticky allow-all of this run covers its children too: they
            // all ask through this one gate.
            return true;
        }
        {
            let mut seq = lock(&self.seq);
            lock(&self.pending).insert(request_id.clone(), respond);
            lock(&self.live_approval).push_back(PendingApproval {
                request_id: request_id.clone(),
                call: call.clone(),
                agent: agent.map(str::to_string),
            });
            self.publish_locked(
                &mut seq,
                Payload::ApprovalRequested {
                    request_id: request_id.clone(),
                    call: call.clone(),
                    agent: agent.map(str::to_string),
                },
            );
            self.publish_locked(
                &mut seq,
                Payload::Notify {
                    kind: NotifyKind::Approval,
                    text: notify_approval_text(&call.name),
                },
            );
        }
        drop(order_guard);
        // The whole round-trip is blocking inside an async context:
        // block_in_place frees the runtime's other workers while this task
        // waits for a decision.
        // A TIMEOUT is a Deny the client never saw resolved: without the
        // frame the desktop's approval strip stays up on a dead request
        // until the next abort (cycle-2 audit M2). An explicit resolve()
        // publishes its own echo - only the timeout path publishes here.
        use std::sync::mpsc::RecvTimeoutError;
        let (allowed, timed_out) =
            match tokio::task::block_in_place(|| receive.recv_timeout(approval_timeout)) {
                Ok(allowed) => (allowed, false),
                Err(RecvTimeoutError::Timeout) => (false, true),
                Err(RecvTimeoutError::Disconnected) => (false, false),
            };
        {
            let mut seq = lock(&self.seq);
            let was_resolved = lock(&self.pending).remove(&request_id).is_none();
            lock(&self.live_approval).retain(|p| p.request_id != request_id);
            if timed_out && !was_resolved {
                self.publish_locked(
                    &mut seq,
                    Payload::ApprovalResolved {
                        request_id: request_id.clone(),
                        decision: modron_protocol::Decision::Deny,
                    },
                );
            }
        }
        allowed
    }

    /// Apply a client's decision (first responder wins). `AllowAll` also
    /// sets the sticky allow-rest flag. Broadcasts the resolution so every
    /// attached client sees it.
    pub fn resolve(&self, request_id: &str, decision: Decision) -> Result<(), String> {
        let mut seq = lock(&self.seq);
        let respond = lock(&self.pending)
            .remove(request_id)
            .ok_or_else(|| format!("unknown or already-resolved request id {request_id}"))?;
        if decision == Decision::AllowAll {
            self.allow_rest.store(true, Ordering::Relaxed);
        }
        lock(&self.live_approval).retain(|p| p.request_id != request_id);
        let _ = respond.send(decision != Decision::Deny);
        self.publish_locked(
            &mut seq,
            Payload::ApprovalResolved {
                request_id: request_id.to_string(),
                decision,
            },
        );
        Ok(())
    }

    /// Abort the active run. Publishes `TurnAborted` exactly once (the
    /// aborted task's own completion path is suppressed via the slot), and
    /// denies every pending approval so a parked approver always releases
    /// — even in the degenerate no-run case, which must not leave a
    /// blocking approver wedged. Returns whether a run was aborted.
    pub fn interrupt(&self) -> bool {
        // The run slot stays locked for the whole interrupt, including the
        // `aborted` store: start_run takes the same lock, so a new run can
        // never start between taking the slot and marking it aborted (LG-c;
        // the old ordering let the new run inherit aborted = true).
        let mut slot = lock(&self.run);
        let task = slot.take();
        let had_run = task.is_some();
        self.busy.store(false, Ordering::Relaxed);
        if let Some(task) = task {
            task.abort();
        }
        // Publish each drained gate's resolution BEFORE TurnAborted so a
        // client never renders a phantom approval for an already-dead run
        // (the gate task could otherwise publish ApprovalRequested after
        // the abort - cycle-2 audit M3).
        // The gate_order lock spans drain + resolutions + the abort frame:
        // a gate either published before we locked (we drain its echo) or
        // sees the empty run slot and publishes nothing (cycle-3 F3).
        let order_guard = lock(&self.gate_order);
        self.aborted.store(true, Ordering::Relaxed);
        // An interrupt discards undelivered steers (the queue, if any, is
        // a separate concept and is not kept here).
        let _discarded = self.steer.drain();
        let mut seq = lock(&self.seq);
        let pending = lock(&self.pending).drain().collect::<Vec<_>>();
        for (request_id, respond) in pending {
            let _ = respond.send(false);
            self.publish_locked(
                &mut seq,
                Payload::ApprovalResolved {
                    request_id,
                    decision: modron_protocol::Decision::Deny,
                },
            );
        }
        lock(&self.live_approval).clear();
        if had_run {
            self.publish_locked(&mut seq, Payload::TurnAborted {});
        }
        drop(seq);
        drop(order_guard);
        drop(slot);
        had_run
    }

    /// Steer one of this session's live sub-agents (H3d; see
    /// [`modron_core::subagent::steer_agent`]). Errors when `agent` is not live.
    pub fn steer_agent(&self, agent: &str, text: String) -> Result<(), String> {
        modron_core::subagent::steer_agent(self.id, agent, text)
    }

    /// Stop one of this session's live sub-agents (H3d; see
    /// [`modron_core::subagent::kill_agent`]). Errors when `agent` is not live.
    pub fn kill_agent(&self, agent: &str) -> Result<(), String> {
        modron_core::subagent::kill_agent(self.id, agent)
    }

    /// Queue a steer for the active run and echo it to subscribers. Holds
    /// the run slot across the push and the publish, so an interrupt either
    /// sees the message (and discards it) or the steer sees no run.
    pub fn steer_message(&self, text: String) -> Result<(), String> {
        let slot = lock(&self.run);
        if slot.is_none() {
            return Err(STEER_IDLE.to_string());
        }
        self.steer.push(text.clone());
        let mut seq = lock(&self.seq);
        self.publish_locked(&mut seq, Payload::Steered { text });
        drop(seq);
        drop(slot);
        Ok(())
    }

    /// The session's title from the store, for the done `Notify`. An async
    /// read: callers must hold no hub lock. `None` when the store has no
    /// such session or the read fails (the done text then uses the id).
    pub async fn notify_title(&self) -> Option<String> {
        match self.shared_store.list().await {
            Ok(metas) => metas
                .into_iter()
                .find(|meta| meta.id == self.id)
                .map(|meta| meta.title),
            Err(error) => {
                tracing::warn!(session = %self.id, %error, "notify title read failed");
                None
            }
        }
    }

    /// The run task calls this after its engine future settles. Whoever
    /// successfully takes the run slot owns the terminal frame; if
    /// [`interrupt`](Self::interrupt) already took it, TurnAborted was
    /// published there and this is a no-op. `title` is the session title
    /// read by [`notify_title`](Self::notify_title) before the slot is taken.
    ///
    /// Returns the front queued message when the run SUCCEEDED and the queue
    /// was non-empty (it is popped and `QueueChanged` is published under the
    /// same locks as TurnDone). The caller must start it after this returns:
    /// every lock and the run slot are released by then. Failed and aborted
    /// runs return `None` and leave the queue intact.
    pub fn complete(&self, end: RunEnd, title: Option<String>) -> Option<String> {
        let mut slot = lock(&self.run);
        // Interrupted: the slot (and the terminal frame) went to the interrupt.
        slot.take()?;
        // Run end (H3b): clear `busy` and deny every parked ask under
        // gate_order, so a background child asking after this point is
        // refused by approve_gate and none parked here outlives the run.
        let order_guard = lock(&self.gate_order);
        self.busy.store(false, Ordering::Relaxed);
        let mut seq = lock(&self.seq);
        let parked = lock(&self.pending).drain().collect::<Vec<_>>();
        for (request_id, respond) in parked {
            let _ = respond.send(false);
            self.publish_locked(
                &mut seq,
                Payload::ApprovalResolved {
                    request_id,
                    decision: modron_protocol::Decision::Deny,
                },
            );
        }
        lock(&self.live_approval).clear();
        drop(order_guard);
        // The slot stays locked through the terminal frame, so a run that
        // starts next cannot publish ahead of this turn's end.
        // Clear the live buffers on the terminal frame: a provider error
        // after text deltas otherwise leaves the stale prefix glued to the
        // NEXT turn's stream for every late attacher (cycle-2 audit L1).
        lock(&self.text).clear();
        lock(&self.reasoning).clear();
        lock(&self.tracker).clear_turn();
        match end {
            RunEnd::Shell(Ok(outcome)) => {
                // A refused command says why; a command that ran shows its
                // output through the persisted Task event, not here.
                if let Some(reason) = outcome.refused_reason.as_deref() {
                    self.publish_locked(
                        &mut seq,
                        Payload::Note {
                            text: format!("shell refused: {reason}"),
                        },
                    );
                }
                self.publish_locked(
                    &mut seq,
                    Payload::TurnDone {
                        summary: shell_summary(self.id, &outcome),
                    },
                );
                self.pop_queued_locked(&mut seq)
            }
            RunEnd::Shell(Err(error)) => {
                self.publish_locked(
                    &mut seq,
                    Payload::TurnFailed {
                        error: error.to_string(),
                    },
                );
                None
            }
            RunEnd::Turn(Ok(summary)) => {
                self.publish_locked(
                    &mut seq,
                    Payload::TurnDone {
                        summary: WireRunSummary::from(&summary),
                    },
                );
                self.publish_locked(
                    &mut seq,
                    Payload::Notify {
                        kind: NotifyKind::Done,
                        text: notify_done_text(self.id, title.as_deref()),
                    },
                );
                // A plan-phase run that produced a plan waits for its
                // decision: the queue stays put until `decide_plan`.
                if self.propose_plan_locked(&mut seq) {
                    return None;
                }
                self.pop_queued_locked(&mut seq)
            }
            RunEnd::Turn(Err(error)) => {
                self.publish_locked(
                    &mut seq,
                    Payload::TurnFailed {
                        error: error.to_string(),
                    },
                );
                self.publish_locked(
                    &mut seq,
                    Payload::Notify {
                        kind: NotifyKind::Failed,
                        text: notify_failed_text(&error.to_string()),
                    },
                );
                None
            }
        }
    }

    /// Hold a message for the next run. Only meaningful while a run is
    /// active: an idle session refuses it (the caller submits instead). The
    /// run slot is held across the push and the publish, like steer.
    pub fn enqueue_message(&self, text: String) -> Result<(), String> {
        let slot = lock(&self.run);
        if slot.is_none() {
            return Err(STEER_IDLE.to_string());
        }
        let mut seq = lock(&self.seq);
        let queued = {
            let mut queue = lock(&self.queue);
            if queue.len() >= QUEUE_CAP {
                return Err(QUEUE_FULL.to_string());
            }
            queue.push_back(text);
            queue.iter().cloned().collect::<Vec<_>>()
        };
        self.publish_locked(&mut seq, Payload::QueueChanged { queued });
        drop(seq);
        drop(slot);
        Ok(())
    }

    /// Remove the queued message at `index` (0 = next to run) and publish
    /// the new list.
    pub fn dequeue_message(&self, index: usize) -> Result<(), String> {
        let mut seq = lock(&self.seq);
        let queued = {
            let mut queue = lock(&self.queue);
            if index >= queue.len() {
                return Err(format!("queue index {index} out of range"));
            }
            queue.remove(index);
            queue.iter().cloned().collect::<Vec<_>>()
        };
        self.publish_locked(&mut seq, Payload::QueueChanged { queued });
        Ok(())
    }

    /// Plan gate (H2b, caller holds the seq lock): a successful run in the
    /// `plan` phase with a non-empty recorded plan proposes it and waits.
    /// Returns true when the plan was proposed.
    fn propose_plan_locked(&self, seq: &mut u64) -> bool {
        let phase = lock(&self.engine).config.phase;
        if phase != Phase::Plan {
            return false;
        }
        let items = lock(&self.plan).clone();
        if items.is_empty() {
            return false;
        }
        self.plan_pending.store(true, Ordering::Relaxed);
        self.publish_locked(seq, Payload::PlanProposed { items });
        self.publish_locked(
            seq,
            Payload::Notify {
                kind: NotifyKind::Plan,
                text: NOTIFY_PLAN_TEXT.to_string(),
            },
        );
        true
    }

    /// Store a computed context gauge and publish `ContextChanged` (H9b).
    /// Stored and published under the seq lock, like every other sink.
    fn set_context(&self, gauge: ContextGauge) {
        let mut seq = lock(&self.seq);
        *lock(&self.context) = Some(gauge);
        self.publish_locked(
            &mut seq,
            Payload::ContextChanged {
                used: gauge.used,
                window: gauge.window,
            },
        );
    }

    /// Recompute the context gauge from the session's log and publish it
    /// (H9b). The engine is cloned under its lock, and that lock is released
    /// before the await, so no hub lock is held across the store read. A
    /// failed read keeps the previous gauge and logs; it never fails a run.
    pub async fn refresh_context(&self) {
        let engine = self.engine_cell().clone();
        match engine.context_gauge(self.id).await {
            Ok(gauge) => self.set_context(gauge),
            Err(error) => {
                tracing::warn!(session = %self.id, %error, "context gauge refresh failed");
            }
        }
    }

    /// [`Self::refresh_context`] without awaiting it, for paths that cannot
    /// await: interrupt aborts the run task, so the run's own refresh never
    /// runs. Does nothing outside a tokio runtime.
    pub fn refresh_context_detached(self: &Arc<Self>) {
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let handle = Arc::clone(self);
            runtime.spawn(async move { handle.refresh_context().await });
        }
    }

    /// Whether a proposed plan awaits a decision (H2b).
    pub fn plan_pending(&self) -> bool {
        self.plan_pending.load(Ordering::Relaxed)
    }

    /// Record a decision on the pending plan (H2b): clears `plan_pending`
    /// and echoes `PlanDecided`. Errors when no plan is pending. The caller
    /// applies the phase change (before this, so the echo order is
    /// ConfigChanged then PlanDecided) and starts the follow-up run.
    pub fn decide_plan(&self, decision: PlanDecision) -> Result<(), String> {
        let mut seq = lock(&self.seq);
        if !self.plan_pending.swap(false, Ordering::Relaxed) {
            return Err(NO_PLAN_PENDING.to_string());
        }
        self.publish_locked(&mut seq, Payload::PlanDecided { decision });
        Ok(())
    }

    /// The armed wake as it reads on the wire (LiveState, persistence).
    pub(crate) fn wake_wire(&self) -> Option<WakeWire> {
        lock(&self.wake).as_ref().map(|armed| armed.wire.clone())
    }

    /// The armed wake's wire when `token` still names it (the timer checks
    /// this before every firing: a replaced or cancelled wake goes quiet).
    pub(crate) fn wake_if_current(&self, token: u64) -> Option<WakeWire> {
        lock(&self.wake)
            .as_ref()
            .filter(|armed| armed.token == token)
            .map(|armed| armed.wire.clone())
    }

    /// Arm `wire` as the session's one wake, replacing (and aborting the
    /// timer of) any armed wake, and publish `WakeChanged`. `start` builds
    /// the timer task; it runs under the seq and wake locks, so the timer
    /// can never observe a half-installed wake.
    pub(crate) fn arm_wake(
        &self,
        token: u64,
        wire: WakeWire,
        start: impl FnOnce() -> JoinHandle<()>,
    ) {
        let mut seq = lock(&self.seq);
        let mut slot = lock(&self.wake);
        if let Some(old) = slot.take() {
            old.task.abort();
        }
        let task = start();
        *slot = Some(ArmedWake {
            wire: wire.clone(),
            token,
            task,
        });
        drop(slot);
        self.publish_locked(&mut seq, Payload::WakeChanged { wake: Some(wire) });
    }

    /// A firing settled. `next` re-arms the same wake (recurring) with its
    /// new time; `None` clears it (one-shot). Returns false, changing
    /// nothing, when `token` is no longer the armed wake.
    pub(crate) fn settle_wake(&self, token: u64, next: Option<WakeWire>) -> bool {
        let mut seq = lock(&self.seq);
        let mut slot = lock(&self.wake);
        if !slot.as_ref().is_some_and(|armed| armed.token == token) {
            return false;
        }
        let wake = match next {
            Some(wire) => {
                if let Some(armed) = slot.as_mut() {
                    armed.wire = wire.clone();
                }
                Some(wire)
            }
            None => {
                *slot = None;
                None
            }
        };
        drop(slot);
        self.publish_locked(&mut seq, Payload::WakeChanged { wake });
        true
    }

    /// Cancel the armed wake: abort its timer and publish
    /// `WakeChanged { wake: None }`. False when none was armed.
    pub(crate) fn cancel_wake(&self) -> bool {
        let mut seq = lock(&self.seq);
        let Some(armed) = lock(&self.wake).take() else {
            return false;
        };
        armed.task.abort();
        self.publish_locked(&mut seq, Payload::WakeChanged { wake: None });
        true
    }

    /// Pop the front follow-up (caller holds the seq lock) and publish the
    /// remaining queue. `None` when the queue is empty.
    fn pop_queued_locked(&self, seq: &mut u64) -> Option<String> {
        let (next, queued) = {
            let mut queue = lock(&self.queue);
            let next = queue.pop_front()?;
            (next, queue.iter().cloned().collect::<Vec<_>>())
        };
        self.publish_locked(seq, Payload::QueueChanged { queued });
        Some(next)
    }

    /// Fold one persisted event into the live view (usage accounting) and
    /// broadcast it as a seq'd `Event` frame.
    pub fn on_persisted(&self, event: &SessionEvent) {
        let mut seq = lock(&self.seq);
        if let SessionEvent::Usage { usage, .. } = event {
            let mut total = lock(&self.usage);
            total.input_tokens += usage.input_tokens;
            total.output_tokens += usage.output_tokens;
            total.cached_input_tokens += usage.cached_input_tokens;
        }
        if let SessionEvent::Plan { items } = event {
            *lock(&self.plan) = items.clone();
        }
        self.publish_locked(
            &mut seq,
            Payload::Event {
                event: event.clone(),
            },
        );
    }

    /// The `explain` lines for a hypothetical call under the session's
    /// current permission rules (the wire's `ExplainRule`), joined by
    /// newlines. Read-only.
    pub fn explain_rule(&self, tool: &str, args: &serde_json::Value) -> String {
        self.engine_cell()
            .config
            .approvals
            .rules
            .explain(tool, args)
            .join("\n")
    }

    /// Mutate the session's engine config for future turns (the wire's
    /// `SetConfig`). The model/provider switch follows the TUI's /model:
    /// provider follows the catalog (or the explicit patch), and the
    /// selector is pinned so the router cannot route elsewhere. Returns a
    /// human note (published as `ConfigChanged` by the caller).
    pub fn apply_config(&self, patch: &ConfigPatch) -> anyhow::Result<()> {
        // Validate and compute everything first, then commit the config, the
        // selector and the status labels together (LG-a): a rejected patch
        // leaves all three exactly as they were. The engine lock spans the
        // whole read-modify-write so two SetConfigs cannot interleave.
        let mut slot = self.engine_cell();
        let mut engine = slot.clone();
        let mut config = engine.config.clone();
        // Label values this patch would set; committed only on success.
        let mut model_label: Option<String> = None;
        let mut provider_label: Option<String> = None;

        if let Some(model) = &patch.model {
            config.default_model = model.clone();
            // Provider follows the model through the catalog unless the
            // patch names one explicitly.
            let provider = patch
                .provider
                .clone()
                .or_else(|| modron_router::lookup(model).map(|info| info.provider.clone()));
            if let Some(provider) = provider {
                config.default_provider = provider;
            }
            // Pin the selector: the router would otherwise keep routing to
            // its own cheap/strong tiers (the live bug the TUI guards).
            engine.selector = Arc::new(modron_core::testing::StaticSelector::new(
                model,
                config.default_provider.clone(),
            ));
            model_label = Some(model.clone());
            provider_label = Some(config.default_provider.clone());
        } else if let Some(provider) = &patch.provider {
            config.default_provider = provider.clone();
            provider_label = Some(provider.clone());
        }
        if let Some(sandbox) = patch.sandbox {
            if !modron_core::sandbox::is_supported(sandbox) {
                anyhow::bail!(
                    "sandbox policy {sandbox:?} is not supported on the server's platform"
                );
            }
            config.sandbox = sandbox;
        }
        if let Some(mode) = patch.approval_mode {
            config.approvals.mode = match mode {
                modron_protocol::ApprovalModeWire::Deny => modron_core::ApprovalMode::Deny,
                modron_protocol::ApprovalModeWire::Ask => modron_core::ApprovalMode::Ask,
                modron_protocol::ApprovalModeWire::ApproveAll => {
                    modron_core::ApprovalMode::ApproveAll
                }
            };
        }
        if let Some(phase) = patch.phase {
            config.phase = phase;
        }
        // `clear_effort` wins over `effort` (see ConfigPatch docs).
        if patch.clear_effort {
            config.effort = None;
        } else if let Some(effort) = patch.effort {
            config.effort = Some(effort);
        }
        // H7c: a roles patch replaces the whole set (all-unset clears it).
        if let Some(roles) = &patch.roles {
            config.roles = (**roles).clone();
        }
        if let Some(allow) = &patch.allow {
            config.approvals.allow = allow.clone();
        }
        if let Some(deny) = &patch.deny {
            config.approvals.deny = deny.clone();
        }
        if let Some(rules) = &patch.rules {
            // Parsed before commit: one bad entry rejects the whole patch.
            config.approvals.rules = parse_rule_set(rules).map_err(anyhow::Error::msg)?;
        }
        if let Some(persona) = &patch.persona {
            // The /agent pattern: system prompt + model/provider/allow/deny
            // follow the configured persona when set.
            let spec = config.personas.get(persona).cloned().ok_or_else(|| {
                anyhow::anyhow!(
                    "unknown persona `{persona}` — configured: {}",
                    if config.personas.is_empty() {
                        "(none)".to_string()
                    } else {
                        config
                            .personas
                            .keys()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                )
            })?;
            if let Some(system) = spec.system {
                config.system = system;
            }
            if let Some(model) = spec.model {
                config.default_model = model.clone();
                model_label = Some(model);
            }
            if let Some(provider) = spec.provider {
                config.default_provider = provider.clone();
                provider_label = Some(provider);
            }
            config.approvals.allow.extend(spec.allow);
            config.approvals.deny.extend(spec.deny);
        }
        let goal_label = patch.goal.clone();

        // Everything validated: commit the config, the selector and the
        // labels in one step.
        engine.config = config;
        *slot = engine;
        if let Some(model) = model_label {
            *lock(&self.model) = model;
        }
        if let Some(provider) = provider_label {
            *lock(&self.provider) = provider;
        }
        if let Some(goal) = goal_label {
            *lock(&self.goal) = goal;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// SessionHub — the map of live sessions
// ---------------------------------------------------------------------------

/// One core rule as its wire form: the effect word and `tool(pattern)`.
pub(crate) fn rule_wire(rule: &Rule) -> RuleWire {
    RuleWire {
        effect: rule.effect.as_str().to_string(),
        spec: rule.spec(),
    }
}

/// Parse a wire rule list into a rule set. The error names the first rule
/// that does not parse; callers reject the whole list on any error.
pub(crate) fn parse_rule_set(rules: &[RuleWire]) -> Result<RuleSet, String> {
    let parsed = rules
        .iter()
        .map(|wire| {
            let effect = match wire.effect.as_str() {
                "allow" => RuleEffect::Allow,
                "ask" => RuleEffect::Ask,
                "deny" => RuleEffect::Deny,
                other => {
                    return Err(format!(
                        "invalid permission rule `{} {}`: unknown effect `{other}` \
                         (use allow, ask or deny)",
                        wire.effect, wire.spec
                    ))
                }
            };
            Rule::parse(effect, &wire.spec).map_err(|err| {
                format!(
                    "invalid permission rule `{} {}`: {err}",
                    wire.effect, wire.spec
                )
            })
        })
        .collect::<Result<Vec<Rule>, String>>()?;
    Ok(RuleSet::new(parsed))
}

/// Cheap-cloneable map of live sessions plus the engine template every
/// per-session engine clones from.
///
/// Cycle note: `SessionHandle.engine.store` is the shared
/// `BroadcastingStore`, which holds the hub — the hub therefore never
/// drops while a handle exists. For a long-lived server process (and the
/// tests' process lifetime) this is harmless.
pub struct SessionHub {
    inner: Arc<HubInner>,
}

struct HubInner {
    sessions: Mutex<HashMap<SessionId, Arc<SessionHandle>>>,
    template: Mutex<Option<Engine>>,
    run_starter: OnceLock<RunStarter>,
}

impl Clone for SessionHub {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl SessionHub {
    /// An empty hub; call [`SessionHub::set_engine`](Self::set_engine)
    /// before creating any session.
    pub fn empty() -> Self {
        Self {
            inner: Arc::new(HubInner {
                sessions: Mutex::new(HashMap::new()),
                template: Mutex::new(None),
                run_starter: OnceLock::new(),
            }),
        }
    }

    /// Install the template engine. Its store must already be the hub's
    /// [`BroadcastingStore`] so every session's appends fan out.
    /// Apply a saved default model and provider (PersistDefaultModel) to the
    /// engine template that FUTURE sessions start from. Existing sessions
    /// keep their model, as with `apply_config`. The template's selector is
    /// pinned to the saved model, the same as a `/model` switch.
    pub fn apply_default_model(&self, model: &str, provider: &str) {
        let Some(mut template) = lock(&self.inner.template).clone() else {
            return;
        };
        template.config.default_model = model.to_string();
        template.config.default_provider = provider.to_string();
        template.selector = Arc::new(modron_core::testing::StaticSelector::new(model, provider));
        self.set_engine(template);
    }

    /// Apply explicit router defaults (onboarding/settings): rebuild the
    /// template selector (heuristic over the chosen tiers) and set the
    /// tier fallbacks. Existing sessions keep their pins.
    pub fn apply_router_defaults(
        &self,
        cheap: &str,
        cheap_provider: &str,
        strong: &str,
        strong_provider: &str,
        cheap_fallback: Option<&str>,
        strong_fallback: Option<&str>,
    ) {
        use modron_core::TierFallbacks;
        let Some(mut template) = lock(&self.inner.template).clone() else {
            return;
        };
        template.config.default_model = strong.to_string();
        template.config.default_provider = strong_provider.to_string();
        template.selector = Arc::new(modron_router::HeuristicSelector::new(
            (cheap.to_string(), cheap_provider.to_string()),
            (strong.to_string(), strong_provider.to_string()),
        ));
        let mut cheap_list = vec![(cheap.to_string(), cheap_provider.to_string())];
        let mut strong_list = vec![(strong.to_string(), strong_provider.to_string())];
        if let Some(fb) = cheap_fallback {
            let provider = modron_router::lookup(fb)
                .map(|i| i.provider.clone())
                .unwrap_or_else(|| cheap_provider.to_string());
            cheap_list.push((fb.to_string(), provider));
        }
        if let Some(fb) = strong_fallback {
            let provider = modron_router::lookup(fb)
                .map(|i| i.provider.clone())
                .unwrap_or_else(|| strong_provider.to_string());
            strong_list.push((fb.to_string(), provider));
        }
        template.config.tier_fallbacks = Some(Arc::new(TierFallbacks {
            cheap: cheap_list,
            strong: strong_list,
        }));
        self.set_engine(template);
    }

    pub fn set_engine(&self, engine: Engine) {
        *lock(&self.inner.template) = Some(engine);
    }

    /// The handle for a live session, if one exists.
    /// Drop a session's live handle (deleted sessions): subscribers keep
    /// their channels but no new frames flow.
    pub fn remove(&self, id: &SessionId) -> Option<Arc<SessionHandle>> {
        lock(&self.inner.sessions).remove(id)
    }

    pub fn handle(&self, id: &SessionId) -> Option<Arc<SessionHandle>> {
        lock(&self.inner.sessions).get(id).cloned()
    }

    /// Get or create the handle for `id`. `pinned` pre-reserves the
    /// session for the next `run_task_with_attachments` (the engine's
    /// `create_session` adopts it); `None` for sessions that will only
    /// ever resume. The per-session engine clone gets the hub's sinks and
    /// a `PinnedCreateStore` over the shared broadcasting store.
    pub fn ensure_session(&self, id: SessionId, pinned: Option<SessionId>) -> Arc<SessionHandle> {
        self.ensure_session_in(id, pinned, None)
    }

    /// [`ensure_session`] with a working-dir override: worktree tasks run
    /// their engine inside the task's worktree.
    pub fn ensure_session_in(
        &self,
        id: SessionId,
        pinned: Option<SessionId>,
        working_dir: Option<PathBuf>,
    ) -> Arc<SessionHandle> {
        let mut sessions = lock(&self.inner.sessions);
        if let Some(existing) = sessions.get(&id) {
            return existing.clone();
        }
        // Startup-ordering regression guard: without a template (no
        // set_engine yet) a session attach must 500 once with an
        // actionable name, not panic inside the connection task forever.
        let template = lock(&self.inner.template).clone().unwrap_or_else(|| {
            panic!("SessionHub::set_engine must run before sessions are created (attach of {id})")
        });
        let shared_store = Arc::clone(&template.store);
        let mut engine = template;
        engine.store = PinnedCreateStore::new(Arc::clone(&shared_store), pinned);

        // Wire this session's sinks onto the config clone: every delta,
        // tool event, and approval routes back here by session id.
        let mut config = engine.config.clone();
        let delta_hub = self.clone();
        config.on_delta = Some(Arc::new(move |delta: StreamDelta| {
            if let Some(handle) = delta_hub.handle(&id) {
                handle.on_stream_delta(delta);
            }
        }));
        let tool_hub = self.clone();
        config.on_tool_event = Some(Arc::new(move |activity: ToolActivity| {
            if let Some(handle) = tool_hub.handle(&id) {
                handle.on_tool_activity(activity);
            }
        }));
        let approval_hub = self.clone();
        config.approver = Some(Arc::new(move |ask: &ApprovalAsk<'_>| {
            approval_hub
                .handle(&id)
                .map(|handle| handle.approve_gate(ask.call, ask.agent))
                .unwrap_or(false)
        }));
        if let Some(dir) = working_dir {
            config.working_dir = dir;
        }
        // One steer inbox per session, owned by the handle. The engine
        // drains it between model turns; `SetConfig` clones this config
        // (apply_config), so the inbox survives config patches.
        let steer = Arc::new(SteerInbox::new());
        config.steer = Some(Arc::clone(&steer));
        engine.config = config;

        let (tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        let handle = Arc::new(SessionHandle {
            id,
            tx,
            seq: Mutex::new(0),
            run: Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
            gate_order: Mutex::new(()),
            aborted: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            deleting: AtomicBool::new(false),
            shared_store,
            approval_timeout: Mutex::new(std::time::Duration::from_secs(30)),
            allow_rest: AtomicBool::new(false),
            tracker: Mutex::new(ActivityTracker::new()),
            text: Mutex::new(String::new()),
            reasoning: Mutex::new(String::new()),
            live_approval: Mutex::new(VecDeque::new()),
            usage: Mutex::new(Usage::default()),
            model: Mutex::new(engine.config.default_model.clone()),
            provider: Mutex::new(engine.config.default_provider.clone()),
            goal: Mutex::new(String::new()),
            engine: Mutex::new(engine),
            steer,
            queue: Mutex::new(VecDeque::new()),
            plan: Mutex::new(Vec::new()),
            plan_pending: AtomicBool::new(false),
            context: Mutex::new(None),
            wake: Mutex::new(None),
        });
        sessions.insert(id, Arc::clone(&handle));
        handle
    }

    /// One persisted event reached the shared store: fold it into the
    /// session's live view and fan it out. Unknown sessions (e.g. events
    /// appended outside the server) are ignored.
    pub fn on_persisted(&self, id: SessionId, event: &SessionEvent) {
        if let Some(handle) = self.handle(&id) {
            handle.on_persisted(event);
        }
    }

    /// Spawn a run for a session: busy-check + slot store are atomic, the
    /// run task itself publishes the terminal frame on completion.
    pub fn start_run(&self, id: SessionId, run: RunFuture) -> Result<(), String> {
        self.spawn_run(id, Box::pin(async move { RunEnd::Turn(run.await) }))
    }

    /// Spawn a user-issued `!command` run (PARITY-C) in the run slot: busy,
    /// interruptible and approval-gated like a turn, with the same terminal
    /// frame (TurnDone / TurnFailed) but no Notify.
    pub fn start_shell_run(&self, id: SessionId, run: ShellRunFuture) -> Result<(), String> {
        self.spawn_run(id, Box::pin(async move { RunEnd::Shell(run.await) }))
    }

    fn spawn_run(
        &self,
        id: SessionId,
        run: Pin<Box<dyn Future<Output = RunEnd> + Send>>,
    ) -> Result<(), String> {
        let handle = self
            .handle(&id)
            .ok_or_else(|| format!("no such session {id}"))?;
        // The run slot lock orders this against interrupt (which holds it
        // across its `aborted` store) and against begin_delete's fence.
        let mut slot = lock(&handle.run);
        if slot.is_some() {
            return Err("session busy (a run is active)".to_string());
        }
        if handle.deleting.load(Ordering::Relaxed) {
            return Err(format!("session {id} is being deleted"));
        }
        {
            let _order = lock(&handle.gate_order);
            handle.aborted.store(false, Ordering::Relaxed);
        }
        // The steer inbox is NOT cleared here. A steer that raced the end of
        // the previous run (pushed while its slot was still held, after the
        // engine's last drain) waits in the inbox and is injected at the
        // start of this run. Only an interrupt discards pending steers.
        // "Allow all" is sticky for the rest of the RUN only (AGENTS.md
        // interaction semantics): a new run starts un-approved.
        handle.allow_rest.store(false, Ordering::Relaxed);
        // Busy before the task can run, so a child ask from its first turn
        // already sees a live run (approve_gate's run-end check).
        handle.busy.store(true, Ordering::Relaxed);
        let owner = Arc::clone(&handle);
        let hub = self.clone();
        let task = tokio::spawn(async move {
            let end = run.await;
            // H9b: the context gauge is recomputed for every run outcome
            // (success or failure) before the terminal frame. An abort never
            // reaches this point; interrupt refreshes on its own path.
            owner.refresh_context().await;
            // The done Notify names the session: the title is read here,
            // with no hub lock held, and only for a successful turn.
            let title = if matches!(end, RunEnd::Turn(Ok(_))) {
                owner.notify_title().await
            } else {
                None
            };
            // `complete` returns only after its run-slot and seq guards are
            // dropped, so the starter may take the slot itself.
            if let Some(next) = owner.complete(end, title) {
                hub.start_queued(owner.id, next);
            }
        });
        *slot = Some(task);
        Ok(())
    }

    /// Install the starter for queued messages (set once by the dispatch
    /// layer at server start; later calls are ignored).
    pub fn set_run_starter(&self, starter: RunStarter) {
        let _ = self.inner.run_starter.set(starter);
    }

    /// Whether a run starter has been installed.
    pub fn has_run_starter(&self) -> bool {
        self.inner.run_starter.get().is_some()
    }

    /// Start a popped follow-up. Runs on the completed run's task with no
    /// hub lock held. Without a starter the message is reported, not lost
    /// silently.
    pub(crate) fn start_queued(&self, id: SessionId, text: String) {
        match self.inner.run_starter.get() {
            Some(starter) => starter(id, text),
            None => {
                if let Some(handle) = self.handle(&id) {
                    handle.publish(Payload::Note {
                        text: format!("queued message not started: no run starter ({text})"),
                    });
                }
            }
        }
    }

    /// Every armed scheduled wake in this hub, for the persisted registry
    /// (H5a).
    pub(crate) fn armed_wakes(&self) -> Vec<(SessionId, WakeWire)> {
        let sessions = lock(&self.inner.sessions);
        sessions
            .values()
            .filter_map(|handle| handle.wake_wire().map(|wire| (handle.id, wire)))
            .collect()
    }

    /// Hold a message for the session's next run (see
    /// [`SessionHandle::enqueue_message`]).
    pub fn enqueue(&self, id: SessionId, text: String) -> Result<(), String> {
        let handle = self
            .handle(&id)
            .ok_or_else(|| format!("no such session {id}"))?;
        handle.enqueue_message(text)
    }

    /// Remove the queued message at `index` (see
    /// [`SessionHandle::dequeue_message`]).
    pub fn dequeue(&self, id: SessionId, index: u32) -> Result<(), String> {
        let handle = self
            .handle(&id)
            .ok_or_else(|| format!("no such session {id}"))?;
        handle.dequeue_message(index as usize)
    }

    /// Steer a session's active run (see [`SessionHandle::steer_message`]).
    /// Errors: no such session, or the session has no active run (submit
    /// instead).
    pub fn steer(&self, id: SessionId, text: String) -> Result<(), String> {
        let handle = self
            .handle(&id)
            .ok_or_else(|| format!("no such session {id}"))?;
        handle.steer_message(text)
    }

    /// Steer a live sub-agent of `id` (see [`SessionHandle::steer_agent`]).
    pub fn steer_agent(&self, id: SessionId, agent: &str, text: String) -> Result<(), String> {
        let handle = self
            .handle(&id)
            .ok_or_else(|| format!("no such session {id}"))?;
        handle.steer_agent(agent, text)
    }

    /// Stop a live sub-agent of `id` (see [`SessionHandle::kill_agent`]).
    pub fn kill_agent(&self, id: SessionId, agent: &str) -> Result<(), String> {
        let handle = self
            .handle(&id)
            .ok_or_else(|| format!("no such session {id}"))?;
        handle.kill_agent(agent)
    }

    /// Start deleting a session. Fails while a run holds its slot. Until
    /// [`finish_delete`](Self::finish_delete) or
    /// [`cancel_delete`](Self::cancel_delete), no run may start: the busy
    /// check and the fence happen under one run-slot lock, so a submit
    /// cannot begin a run between the check and the removal.
    pub fn begin_delete(&self, id: SessionId) -> Result<(), String> {
        let handle = self.ensure_session(id, None);
        let slot = lock(&handle.run);
        if slot.is_some() {
            return Err("session is running - interrupt it before deleting".to_string());
        }
        handle.deleting.store(true, Ordering::Relaxed);
        drop(slot);
        Ok(())
    }

    /// The delete failed: lift the fence, keep the session.
    pub fn cancel_delete(&self, id: SessionId) {
        if let Some(handle) = self.handle(&id) {
            let _slot = lock(&handle.run);
            handle.deleting.store(false, Ordering::Relaxed);
        }
    }

    /// The session is gone from the store: drop its live handle.
    pub fn finish_delete(&self, id: SessionId) {
        self.remove(&id);
    }

    /// Apply an approval decision wherever the request lives (first
    /// responder wins; the request ids are globally unique). Returns the
    /// session that owned it.
    pub fn resolve_any(&self, request_id: &str, decision: Decision) -> Result<SessionId, String> {
        let sessions = lock(&self.inner.sessions);
        for handle in sessions.values() {
            if handle.resolve(request_id, decision).is_ok() {
                return Ok(handle.id);
            }
        }
        Err(format!(
            "unknown or already-resolved request id {request_id}"
        ))
    }
}

/// The terminal `TurnDone` summary of a user-issued shell run: no answer,
/// no turns, one tool call when the command ran.
fn shell_summary(id: SessionId, outcome: &UserShellOutcome) -> WireRunSummary {
    WireRunSummary {
        session_id: id,
        answer: String::new(),
        turns: 0,
        tool_calls: u32::from(outcome.allowed),
        usage: Usage::default(),
        models_used: Vec::new(),
    }
}

/// Text of the `Notify` a plan proposal sends.
pub(crate) const NOTIFY_PLAN_TEXT: &str = "plan ready for review";

/// Characters of a failed run's first error line kept in its `Notify`.
const NOTIFY_FAILED_CHARS: usize = 120;

/// Characters of a wake's prompt kept in its `Notify`.
const NOTIFY_WAKE_CHARS: usize = 80;

/// Characters of a session title kept in a done `Notify`.
const NOTIFY_TITLE_CHARS: usize = 60;

/// Notify text for a finished run: `done: <title>` cut to 60 chars, or
/// `done: <short session id>` when the session has no title. Every `Notify`
/// text is built here, so the wording lives in one place (H5a).
pub(crate) fn notify_done_text(session: SessionId, title: Option<&str>) -> String {
    match title.map(str::trim).filter(|title| !title.is_empty()) {
        Some(title) => format!(
            "done: {}",
            title.chars().take(NOTIFY_TITLE_CHARS).collect::<String>()
        ),
        None => {
            let id = session.to_string();
            format!("done: {}", &id[..8])
        }
    }
}

/// Notify text for a pending approval: `approval needed: <tool name>`. The
/// tool's arguments never go here: the text becomes a phone push.
pub(crate) fn notify_approval_text(tool: &str) -> String {
    format!("approval needed: {tool}")
}

/// Notify text for a failed run: its first error line, cut to 120 chars.
pub(crate) fn notify_failed_text(error: &str) -> String {
    let first = error.lines().next().unwrap_or("");
    format!(
        "failed: {}",
        first.chars().take(NOTIFY_FAILED_CHARS).collect::<String>()
    )
}

/// Notify text for a finished background sub-agent (H3d): the shared view
/// wording, `sub-agent <label> finished` or `... failed`.
pub(crate) fn notify_agent_text(label: &str, ok: bool) -> String {
    modron_view::agent_finished_text(label, ok)
}

/// Notify text for a fired wake: the first 80 chars of its prompt.
pub(crate) fn notify_wake_text(prompt: &str) -> String {
    format!(
        "wake: {}",
        prompt.chars().take(NOTIFY_WAKE_CHARS).collect::<String>()
    )
}

/// Session title for a submitted task (mirror of the engine's own rule:
/// first line, 60 chars, ellipsis on truncation) — used when the server
/// reserves the session before the engine's create call.
pub fn session_title(task: &str) -> String {
    const TITLE_MAX_CHARS: usize = 60;
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

/// The standing-goal wrapper (the TUI's `task_with_goal`): prepends the
/// goal so the model keeps working across turns until it is met.
pub fn task_with_goal(goal: &str, task: &str) -> String {
    if goal.is_empty() {
        task.to_string()
    } else {
        format!(
            "{task}\n\n(Goal for this run: {goal}. Keep working across turns until it \
             is met, then end your final message with the exact line: GOAL COMPLETE)"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use modron_core::testing::{InMemoryStore, StaticSelector};
    use modron_core::{EngineConfig, ToolRegistry};
    use std::time::Duration;

    fn test_hub() -> SessionHub {
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
        hub
    }

    /// A run that parks until the test interrupts it.
    fn parked_run(id: SessionId) -> RunFuture {
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

    /// The handle's engine config carries the handle's own inbox.
    #[test]
    fn session_engine_config_carries_the_handle_steer_inbox() {
        let hub = test_hub();
        let id = SessionId::new_v4();
        let handle = hub.ensure_session(id, None);
        let config_inbox = lock(&handle.engine)
            .config
            .steer
            .clone()
            .expect("steer wired");
        assert!(Arc::ptr_eq(&config_inbox, &handle.steer));
    }

    #[test]
    fn steer_on_an_idle_session_is_refused() {
        let hub = test_hub();
        let id = SessionId::new_v4();
        hub.ensure_session(id, None);
        let err = hub.steer(id, "change course".into()).unwrap_err();
        assert_eq!(err, STEER_IDLE);
        assert!(hub.handle(&id).unwrap().steer.is_empty());
        assert!(hub.steer(SessionId::new_v4(), "x".into()).is_err());
    }

    #[tokio::test]
    async fn steer_on_a_busy_session_queues_and_publishes_steered() {
        let hub = test_hub();
        let id = SessionId::new_v4();
        let handle = hub.ensure_session(id, None);
        let mut rx = handle.tx.subscribe();
        hub.start_run(id, parked_run(id)).unwrap();
        hub.steer(id, "use the other crate".into()).unwrap();
        assert_eq!(
            handle.steer.drain(),
            vec!["use the other crate".to_string()]
        );
        let frame = rx.recv().await.expect("live frame");
        let json: serde_json::Value = serde_json::from_str(&frame.json).unwrap();
        assert_eq!(json["type"], "steered");
        assert_eq!(json["text"], "use the other crate");
        handle.interrupt();
    }

    #[tokio::test]
    async fn interrupt_discards_pending_steers() {
        let hub = test_hub();
        let id = SessionId::new_v4();
        let handle = hub.ensure_session(id, None);
        hub.start_run(id, parked_run(id)).unwrap();
        hub.steer(id, "first".into()).unwrap();
        assert!(handle.interrupt());
        assert!(handle.steer.is_empty());
    }

    /// H9b: a finished run publishes `ContextChanged` before its `TurnDone`,
    /// and the gauge rides `LiveState` (what an attach snapshot carries).
    #[tokio::test]
    async fn a_finished_run_publishes_and_reports_the_context_gauge() {
        let store = Arc::new(InMemoryStore::new());
        let hub = SessionHub::empty();
        hub.set_engine(Engine {
            selector: Arc::new(StaticSelector::new("mock-model", "mock")),
            providers: Default::default(),
            tools: Arc::new(ToolRegistry::new()),
            store: store.clone(),
            config: EngineConfig {
                default_model: "mock-model".into(),
                default_provider: "mock".into(),
                ..EngineConfig::default()
            },
        });
        let id = store.create_session("gauge").await.unwrap().id;
        let handle = hub.ensure_session(id, None);
        assert!(
            handle.live_state().context.is_none(),
            "no gauge before a run"
        );

        let mut rx = handle.tx.subscribe();
        let (done, gate) = tokio::sync::oneshot::channel();
        hub.start_run(id, gated_run(id, gate)).unwrap();
        done.send(()).unwrap();

        let mut saw_gauge = false;
        loop {
            let frame = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("a live frame")
                .expect("channel open");
            let json: serde_json::Value = serde_json::from_str(&frame.json).unwrap();
            match json["type"].as_str() {
                Some("context_changed") => saw_gauge = true,
                Some("turn_done") => break,
                _ => {}
            }
        }
        assert!(saw_gauge, "ContextChanged precedes TurnDone");
        let context = handle.live_state().context.expect("gauge in live state");
        assert!(
            context.used > 0,
            "system prompt and tool specs always count"
        );
        assert!(context.window > 0, "the window is never zero");
    }

    /// Frames up to and including the first `notify` of `kind`.
    async fn until_notify(
        rx: &mut broadcast::Receiver<LiveFrame>,
        kind: &str,
    ) -> Vec<serde_json::Value> {
        tokio::time::timeout(Duration::from_secs(2), async {
            let mut seen = Vec::new();
            loop {
                let frame = rx.recv().await.expect("live frame");
                let json: serde_json::Value = serde_json::from_str(&frame.json).unwrap();
                let hit = json["type"] == "notify" && json["kind"] == kind;
                seen.push(json);
                if hit {
                    return seen;
                }
            }
        })
        .await
        .expect("notify arrives")
    }

    /// The `notify` frames among `frames`, as (kind, text) pairs.
    fn notify_pairs(frames: &[serde_json::Value]) -> Vec<(String, String)> {
        frames
            .iter()
            .filter(|f| f["type"] == "notify")
            .map(|f| {
                (
                    f["kind"].as_str().unwrap().to_string(),
                    f["text"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    /// H3d: a background child's settlement pushes `AgentFinished` and its
    /// `Notify` (done for success, failed for a failure or a kill), in that
    /// order, and no raw `tool` frame.
    #[tokio::test]
    async fn a_background_child_finish_pushes_agent_finished_and_notify() {
        let hub = test_hub();
        let id = SessionId::new_v4();
        let handle = hub.ensure_session(id, None);
        let mut rx = handle.subscribe();
        handle.on_tool_activity(ToolActivity::BackgroundFinished {
            id: "a1".into(),
            label: "delegate".into(),
            ok: true,
            brief: "all good".into(),
        });
        handle.on_tool_activity(ToolActivity::BackgroundFinished {
            id: "a2".into(),
            label: "persona".into(),
            ok: false,
            brief: "stopped by the user".into(),
        });
        let mut frames = Vec::new();
        for _ in 0..4 {
            let frame = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("frame arrives")
                .expect("live frame");
            frames.push(serde_json::from_str::<serde_json::Value>(&frame.json).unwrap());
        }
        assert_eq!(frames[0]["type"], "agent_finished");
        assert_eq!(frames[0]["agent"], "a1");
        assert_eq!(frames[0]["ok"], true);
        assert_eq!(frames[0]["brief"], "all good");
        assert_eq!(frames[1]["type"], "notify");
        assert_eq!(frames[2]["type"], "agent_finished");
        assert_eq!(frames[2]["ok"], false);
        assert_eq!(
            notify_pairs(&frames),
            vec![
                (
                    "done".to_string(),
                    "sub-agent delegate finished".to_string()
                ),
                ("failed".to_string(), "sub-agent persona failed".to_string()),
            ]
        );
        assert!(
            frames.iter().all(|f| f["type"] != "tool"),
            "a settlement is a push, not a tool frame"
        );
    }

    /// H3d: steering or stopping an agent that is not live is refused with the
    /// requester's error, and an unknown session is refused too.
    #[tokio::test]
    async fn steer_and_kill_agent_refuse_unknown_agents_and_sessions() {
        let hub = test_hub();
        let id = SessionId::new_v4();
        hub.ensure_session(id, None);
        assert!(hub
            .steer_agent(id, "nobody", "x".into())
            .unwrap_err()
            .contains("no live sub-agent"));
        assert!(hub
            .kill_agent(id, "nobody")
            .unwrap_err()
            .contains("no live sub-agent"));
        assert!(hub
            .kill_agent(SessionId::new_v4(), "nobody")
            .unwrap_err()
            .contains("no such session"));
    }

    #[tokio::test]
    async fn a_finished_run_notifies_done() {
        let hub = test_hub();
        let id = SessionId::new_v4();
        let handle = hub.ensure_session(id, None);
        let (done, gate) = tokio::sync::oneshot::channel();
        hub.start_run(id, gated_run(id, gate)).unwrap();
        let mut rx = handle.subscribe();
        done.send(()).unwrap();
        let frames = until_notify(&mut rx, "done").await;
        assert!(frames.iter().any(|f| f["type"] == "turn_done"));
        let expected = notify_done_text(id, None);
        assert_eq!(
            notify_pairs(&frames),
            vec![("done".to_string(), expected.clone())]
        );
        assert!(expected.starts_with("done: ") && expected.len() == "done: ".len() + 8);
    }

    #[tokio::test]
    async fn a_finished_run_names_the_session_by_title() {
        let store: Arc<dyn modron_core::SessionStore> = Arc::new(InMemoryStore::new());
        let hub = SessionHub::empty();
        hub.set_engine(Engine {
            selector: Arc::new(StaticSelector::new("mock-model", "mock")),
            providers: Default::default(),
            tools: Arc::new(ToolRegistry::new()),
            store: store.clone(),
            config: EngineConfig {
                default_model: "mock-model".into(),
                default_provider: "mock".into(),
                ..EngineConfig::default()
            },
        });
        let title = "x".repeat(80);
        let id = store.create_session(&title).await.unwrap().id;
        let handle = hub.ensure_session(id, None);
        let (done, gate) = tokio::sync::oneshot::channel();
        hub.start_run(id, gated_run(id, gate)).unwrap();
        let mut rx = handle.subscribe();
        done.send(()).unwrap();
        let frames = until_notify(&mut rx, "done").await;
        assert_eq!(
            notify_pairs(&frames),
            vec![("done".to_string(), format!("done: {}", "x".repeat(60)))]
        );
        assert_eq!(handle.notify_title().await.as_deref(), Some(title.as_str()));
    }

    #[test]
    fn done_text_uses_the_title_when_present_and_the_id_otherwise() {
        let id = SessionId::new_v4();
        let short = &id.to_string()[..8];
        assert_eq!(notify_done_text(id, None), format!("done: {short}"));
        assert_eq!(notify_done_text(id, Some("   ")), format!("done: {short}"));
        assert_eq!(
            notify_done_text(id, Some("fix the parser")),
            "done: fix the parser"
        );
    }

    #[tokio::test]
    async fn a_failed_run_notifies_failed_with_the_first_error_line() {
        let hub = test_hub();
        let id = SessionId::new_v4();
        let handle = hub.ensure_session(id, None);
        let (done, gate) = tokio::sync::oneshot::channel();
        hub.start_run(id, failing_run(gate)).unwrap();
        let mut rx = handle.subscribe();
        done.send(()).unwrap();
        let frames = until_notify(&mut rx, "failed").await;
        assert!(frames.iter().any(|f| f["type"] == "turn_failed"));
        let error = modron_core::Error::Session("boom".into()).to_string();
        assert_eq!(
            notify_pairs(&frames),
            vec![("failed".to_string(), notify_failed_text(&error))]
        );
    }

    #[test]
    fn notify_texts_are_built_in_one_place_and_cut_short() {
        let long_error = format!("{}\nsecond line", "e".repeat(200));
        let failed = notify_failed_text(&long_error);
        assert_eq!(failed, format!("failed: {}", "e".repeat(120)));
        let wake = notify_wake_text(&"w".repeat(100));
        assert_eq!(wake, format!("wake: {}", "w".repeat(80)));
        assert_eq!(NOTIFY_PLAN_TEXT, "plan ready for review");
    }

    #[tokio::test]
    async fn a_proposed_plan_notifies_plan_ready() {
        let hub = test_hub();
        let id = SessionId::new_v4();
        let handle = hub.ensure_session(id, None);
        handle
            .apply_config(&ConfigPatch {
                phase: Some(Phase::Plan),
                ..ConfigPatch::default()
            })
            .unwrap();
        let (done, gate) = tokio::sync::oneshot::channel();
        hub.start_run(id, gated_run(id, gate)).unwrap();
        handle.on_persisted(&modron_core::SessionEvent::Plan {
            items: vec![modron_core::PlanItem {
                text: "add the parser".into(),
                done_criteria: "parser tests pass".into(),
                status: "pending".into(),
            }],
        });
        let mut rx = handle.subscribe();
        done.send(()).unwrap();
        let frames = until_notify(&mut rx, "plan").await;
        assert!(frames.iter().any(|f| f["type"] == "plan_proposed"));
        // The plan run ended (done), then waits for a decision (plan).
        assert_eq!(
            notify_pairs(&frames),
            vec![
                ("done".to_string(), notify_done_text(id, None)),
                ("plan".to_string(), NOTIFY_PLAN_TEXT.to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn an_interrupted_run_notifies_nothing() {
        let hub = test_hub();
        let id = SessionId::new_v4();
        let handle = hub.ensure_session(id, None);
        hub.start_run(id, parked_run(id)).unwrap();
        let mut rx = handle.subscribe();
        assert!(handle.interrupt());
        let frames = tokio::time::timeout(Duration::from_secs(2), async {
            let mut seen = Vec::new();
            loop {
                let frame = rx.recv().await.expect("live frame");
                let json: serde_json::Value = serde_json::from_str(&frame.json).unwrap();
                let end = json["type"] == "turn_aborted";
                seen.push(json);
                if end {
                    return seen;
                }
            }
        })
        .await
        .expect("turn_aborted arrives");
        assert!(notify_pairs(&frames).is_empty(), "abort is not a notify");
    }

    /// A run that finishes successfully when the test sends on `done`.
    fn gated_run(id: SessionId, done: tokio::sync::oneshot::Receiver<()>) -> RunFuture {
        Box::pin(async move {
            let _ = done.await;
            Ok(RunSummary {
                session_id: id,
                answer: "ok".into(),
                turns: 1,
                tool_calls: 0,
                usage: Default::default(),
                models_used: vec![],
            })
        })
    }

    /// A run that fails when the test sends on `done`.
    fn failing_run(done: tokio::sync::oneshot::Receiver<()>) -> RunFuture {
        Box::pin(async move {
            let _ = done.await;
            Err(modron_core::Error::Session("boom".into()))
        })
    }

    /// Starter that forwards every starter call to a channel the test reads.
    fn recording_starter(
        hub: &SessionHub,
    ) -> tokio::sync::mpsc::UnboundedReceiver<(SessionId, String)> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        hub.set_run_starter(Arc::new(move |id, text| {
            let _ = tx.send((id, text));
        }));
        rx
    }

    fn queued_of(handle: &SessionHandle) -> Vec<String> {
        handle.live_state().queued
    }

    #[tokio::test]
    async fn enqueue_on_a_busy_session_publishes_and_shows_in_live_state() {
        let hub = test_hub();
        let id = SessionId::new_v4();
        let handle = hub.ensure_session(id, None);
        let mut rx = handle.tx.subscribe();
        hub.start_run(id, parked_run(id)).unwrap();
        hub.enqueue(id, "first follow-up".into()).unwrap();
        hub.enqueue(id, "second".into()).unwrap();
        assert_eq!(
            queued_of(&handle),
            vec!["first follow-up".to_string(), "second".to_string()]
        );
        let frame = rx.recv().await.expect("live frame");
        let json: serde_json::Value = serde_json::from_str(&frame.json).unwrap();
        assert_eq!(json["type"], "queue_changed");
        assert_eq!(json["queued"], serde_json::json!(["first follow-up"]));
        handle.interrupt();
    }

    #[tokio::test]
    async fn enqueue_on_an_idle_session_is_refused() {
        let hub = test_hub();
        let id = SessionId::new_v4();
        let handle = hub.ensure_session(id, None);
        let err = hub.enqueue(id, "later".into()).unwrap_err();
        assert_eq!(err, STEER_IDLE);
        assert!(queued_of(&handle).is_empty());
    }

    #[tokio::test]
    async fn dequeue_removes_the_indexed_entry_and_rejects_out_of_range() {
        let hub = test_hub();
        let id = SessionId::new_v4();
        let handle = hub.ensure_session(id, None);
        hub.start_run(id, parked_run(id)).unwrap();
        for text in ["a", "b", "c"] {
            hub.enqueue(id, text.into()).unwrap();
        }
        hub.dequeue(id, 1).unwrap();
        assert_eq!(queued_of(&handle), vec!["a".to_string(), "c".to_string()]);
        assert!(hub.dequeue(id, 2).is_err());
        assert_eq!(queued_of(&handle).len(), 2);
        assert!(hub.dequeue(SessionId::new_v4(), 0).is_err());
        handle.interrupt();
    }

    #[tokio::test]
    async fn queue_cap_is_enforced() {
        let hub = test_hub();
        let id = SessionId::new_v4();
        let handle = hub.ensure_session(id, None);
        hub.start_run(id, parked_run(id)).unwrap();
        for n in 0..QUEUE_CAP {
            hub.enqueue(id, format!("m{n}")).unwrap();
        }
        assert_eq!(hub.enqueue(id, "overflow".into()).unwrap_err(), QUEUE_FULL);
        assert_eq!(queued_of(&handle).len(), QUEUE_CAP);
        handle.interrupt();
    }

    #[tokio::test]
    async fn successful_run_starts_the_front_queued_message() {
        let hub = test_hub();
        let mut starts = recording_starter(&hub);
        let id = SessionId::new_v4();
        let handle = hub.ensure_session(id, None);
        let (done, gate) = tokio::sync::oneshot::channel();
        hub.start_run(id, gated_run(id, gate)).unwrap();
        hub.enqueue(id, "next".into()).unwrap();
        hub.enqueue(id, "after".into()).unwrap();
        done.send(()).unwrap();
        let (started_id, text) = starts.recv().await.expect("starter invoked");
        assert_eq!(started_id, id);
        assert_eq!(text, "next");
        assert_eq!(queued_of(&handle), vec!["after".to_string()]);
    }

    #[tokio::test]
    async fn failed_or_aborted_run_keeps_the_queue_and_skips_the_starter() {
        let hub = test_hub();
        let mut starts = recording_starter(&hub);
        let id = SessionId::new_v4();
        let handle = hub.ensure_session(id, None);

        // Failure: TurnFailed, no start, queue intact.
        let (done, gate) = tokio::sync::oneshot::channel();
        hub.start_run(id, failing_run(gate)).unwrap();
        hub.enqueue(id, "kept".into()).unwrap();
        done.send(()).unwrap();
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        assert!(!handle.is_busy());
        assert_eq!(queued_of(&handle), vec!["kept".to_string()]);

        // Interrupt: TurnAborted, no start, queue intact.
        hub.start_run(id, parked_run(id)).unwrap();
        assert!(handle.interrupt());
        assert_eq!(queued_of(&handle), vec!["kept".to_string()]);
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        assert!(starts.try_recv().is_err(), "starter must not run");
    }

    fn plan_items() -> Vec<PlanItem> {
        vec![PlanItem {
            text: "add the parser".into(),
            done_criteria: "parser tests pass".into(),
            status: "pending".into(),
        }]
    }

    /// Read live frames until one of `kind` arrives (2s budget).
    async fn next_frame_of(
        rx: &mut broadcast::Receiver<LiveFrame>,
        kind: &str,
    ) -> serde_json::Value {
        tokio::time::timeout(Duration::from_secs(2), async {
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

    /// A phase patch sets the engine config for future runs and shows in
    /// the live state; a patch without `phase` leaves it alone.
    #[test]
    fn phase_patch_reaches_the_engine_config_and_live_state() {
        let hub = test_hub();
        let id = SessionId::new_v4();
        let handle = hub.ensure_session(id, None);
        assert_eq!(handle.live_state().phase, Some(Phase::Build));
        handle
            .apply_config(&ConfigPatch {
                phase: Some(Phase::Plan),
                ..ConfigPatch::default()
            })
            .unwrap();
        assert_eq!(handle.engine_cell().config.phase, Phase::Plan);
        assert_eq!(handle.live_state().phase, Some(Phase::Plan));
        handle
            .apply_config(&ConfigPatch {
                model: Some("other-model".into()),
                ..ConfigPatch::default()
            })
            .unwrap();
        assert_eq!(handle.engine_cell().config.phase, Phase::Plan);
    }

    /// H7c: a roles patch replaces the engine's roles and reaches the live
    /// state; a patch without roles leaves them alone; an empty value clears.
    #[test]
    fn roles_patch_reaches_the_engine_config_and_live_state() {
        use modron_core::ModelRoles;
        let hub = test_hub();
        let id = SessionId::new_v4();
        let handle = hub.ensure_session(id, None);
        assert!(handle.live_state().roles.is_empty());
        let roles = ModelRoles {
            plan: Some("plan-model".into()),
            critic: Some("critic-model".into()),
            ..ModelRoles::default()
        };
        handle
            .apply_config(&ConfigPatch {
                roles: Some(Box::new(roles.clone())),
                ..ConfigPatch::default()
            })
            .unwrap();
        assert_eq!(handle.engine_cell().config.roles, roles);
        assert_eq!(handle.live_state().roles, roles);
        handle
            .apply_config(&ConfigPatch {
                model: Some("other-model".into()),
                ..ConfigPatch::default()
            })
            .unwrap();
        assert_eq!(handle.engine_cell().config.roles, roles);
        handle
            .apply_config(&ConfigPatch {
                roles: Some(Box::new(ModelRoles::default())),
                ..ConfigPatch::default()
            })
            .unwrap();
        assert!(handle.engine_cell().config.roles.is_empty());
        assert!(handle.live_state().roles.is_empty());
    }

    /// H7a: an effort patch reaches the engine config (future turns) and the
    /// live state; `clear_effort` returns it to the provider default; a patch
    /// without effort fields leaves it alone.
    #[test]
    fn effort_patch_reaches_the_engine_config_and_live_state() {
        use modron_core::Effort;
        let hub = test_hub();
        let id = SessionId::new_v4();
        let handle = hub.ensure_session(id, None);
        assert_eq!(handle.live_state().effort, None);
        handle
            .apply_config(&ConfigPatch {
                effort: Some(Effort::High),
                ..ConfigPatch::default()
            })
            .unwrap();
        assert_eq!(handle.engine_cell().config.effort, Some(Effort::High));
        assert_eq!(handle.live_state().effort, Some(Effort::High));
        handle
            .apply_config(&ConfigPatch {
                model: Some("other-model".into()),
                ..ConfigPatch::default()
            })
            .unwrap();
        assert_eq!(handle.engine_cell().config.effort, Some(Effort::High));
        handle
            .apply_config(&ConfigPatch {
                clear_effort: true,
                ..ConfigPatch::default()
            })
            .unwrap();
        assert_eq!(handle.engine_cell().config.effort, None);
        assert_eq!(handle.live_state().effort, None);
    }

    /// A successful plan-phase run with a recorded plan proposes it, sets
    /// plan_pending, and does NOT start the queued follow-up.
    #[tokio::test]
    async fn plan_run_with_a_recorded_plan_proposes_it_and_holds_the_queue() {
        let hub = test_hub();
        let mut starts = recording_starter(&hub);
        let id = SessionId::new_v4();
        let handle = hub.ensure_session(id, None);
        handle
            .apply_config(&ConfigPatch {
                phase: Some(Phase::Plan),
                ..ConfigPatch::default()
            })
            .unwrap();
        let mut rx = handle.tx.subscribe();
        let (done, gate) = tokio::sync::oneshot::channel();
        hub.start_run(id, gated_run(id, gate)).unwrap();
        hub.enqueue(id, "next".into()).unwrap();
        handle.on_persisted(&SessionEvent::Plan {
            items: plan_items(),
        });
        done.send(()).unwrap();

        let proposed = next_frame_of(&mut rx, "plan_proposed").await;
        assert_eq!(proposed["items"][0]["text"], "add the parser");
        assert!(handle.plan_pending());
        assert!(handle.live_state().plan_pending);
        assert_eq!(queued_of(&handle), vec!["next".to_string()]);
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        assert!(starts.try_recv().is_err(), "queued start must wait");
    }

    /// A build-phase run publishes no proposal even with a recorded plan,
    /// and the queue starts as before.
    #[tokio::test]
    async fn build_phase_run_publishes_no_plan_proposal() {
        let hub = test_hub();
        let mut starts = recording_starter(&hub);
        let id = SessionId::new_v4();
        let handle = hub.ensure_session(id, None);
        let mut rx = handle.tx.subscribe();
        let (done, gate) = tokio::sync::oneshot::channel();
        hub.start_run(id, gated_run(id, gate)).unwrap();
        hub.enqueue(id, "next".into()).unwrap();
        handle.on_persisted(&SessionEvent::Plan {
            items: plan_items(),
        });
        done.send(()).unwrap();

        let (started, text) = starts.recv().await.expect("queued run starts");
        assert_eq!(started, id);
        assert_eq!(text, "next");
        assert!(!handle.plan_pending());
        while let Ok(frame) = rx.try_recv() {
            let json: serde_json::Value = serde_json::from_str(&frame.json).unwrap();
            assert_ne!(json["type"], "plan_proposed");
        }
    }

    /// A plan-phase run with no recorded plan proposes nothing.
    #[tokio::test]
    async fn plan_run_without_a_plan_proposes_nothing() {
        let hub = test_hub();
        let id = SessionId::new_v4();
        let handle = hub.ensure_session(id, None);
        handle
            .apply_config(&ConfigPatch {
                phase: Some(Phase::Plan),
                ..ConfigPatch::default()
            })
            .unwrap();
        let (done, gate) = tokio::sync::oneshot::channel();
        hub.start_run(id, gated_run(id, gate)).unwrap();
        done.send(()).unwrap();
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        assert!(!handle.is_busy());
        assert!(!handle.plan_pending());
    }

    /// A decision with nothing pending is refused and changes nothing.
    #[test]
    fn decide_plan_with_nothing_pending_is_refused() {
        let hub = test_hub();
        let id = SessionId::new_v4();
        let handle = hub.ensure_session(id, None);
        assert_eq!(
            handle.decide_plan(PlanDecision::Approve).unwrap_err(),
            NO_PLAN_PENDING
        );
        assert!(!handle.plan_pending());
    }

    /// decide_plan clears the pending flag, echoes PlanDecided, and only
    /// succeeds once.
    #[tokio::test]
    async fn decide_plan_clears_pending_and_publishes_once() {
        let hub = test_hub();
        let id = SessionId::new_v4();
        let handle = hub.ensure_session(id, None);
        handle
            .apply_config(&ConfigPatch {
                phase: Some(Phase::Plan),
                ..ConfigPatch::default()
            })
            .unwrap();
        let (done, gate) = tokio::sync::oneshot::channel();
        let mut rx = handle.tx.subscribe();
        hub.start_run(id, gated_run(id, gate)).unwrap();
        handle.on_persisted(&SessionEvent::Plan {
            items: plan_items(),
        });
        done.send(()).unwrap();
        next_frame_of(&mut rx, "plan_proposed").await;

        handle.decide_plan(PlanDecision::Revise).unwrap();
        assert!(!handle.plan_pending());
        assert!(!handle.live_state().plan_pending);
        let decided = next_frame_of(&mut rx, "plan_decided").await;
        assert_eq!(decided["decision"], "revise");
        assert!(handle.decide_plan(PlanDecision::Approve).is_err());
    }
}
