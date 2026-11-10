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

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use modron_core::{
    Engine, RunSummary, SessionEvent, SessionId, SessionMeta, SessionStore, StreamDelta,
    ToolActivity, ToolCall, Usage,
};
use modron_protocol::{
    ConfigPatch, Decision, Frame, LiveState, Payload, PendingApproval, WireDelta, WireRunSummary,
    WireSwarmRow, WireToolActivity,
};
use modron_view::ActivityTracker;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use uuid::Uuid;

/// Broadcast-channel capacity per session (frames, not bytes). A client
/// slower than 1024 queued frames is lagged and must re-attach.
const CHANNEL_CAPACITY: usize = 1024;

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
        self.hub.on_persisted(id, event.clone());
        Ok(())
    }

    async fn load(&self, id: SessionId) -> modron_core::Result<Vec<SessionEvent>> {
        self.inner.load(id).await
    }

    async fn list(&self) -> modron_core::Result<Vec<SessionMeta>> {
        self.inner.list().await
    }

    async fn delete(&self, id: SessionId) -> modron_core::Result<bool> {
        self.inner.delete(id).await
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
        for event in &events {
            self.inner.append(meta.id, event).await?;
        }
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
                let existing = self
                    .inner
                    .list()
                    .await?
                    .into_iter()
                    .find(|meta| meta.id == id);
                match existing {
                    Some(meta) => Ok(meta),
                    None => self.inner.create_session(title).await,
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
}

// ---------------------------------------------------------------------------
// SessionHandle — one live session
// ---------------------------------------------------------------------------

/// The boxed engine future a `Submit` produces (which `Engine::run_task*`
/// variant to drive for this run).
pub type RunFuture = Pin<Box<dyn Future<Output = modron_core::Result<RunSummary>> + Send>>;

/// One live session: broadcast channel + seq counter + run state + the
/// per-session [`Engine`] clone carrying the hub's sinks.
///
/// Field locking discipline: every mutex is short-lived and never held
/// across an await. `seq` guards publish ordering; `run` doubles as the
/// busy flag AND the terminal-frame lock (see [`SessionHandle::complete`]).
pub struct SessionHandle {
    pub id: SessionId,
    tx: broadcast::Sender<Frame>,
    seq: Mutex<u64>,
    /// The active run's task; `Some` = busy. Taken by whoever emits the
    /// terminal frame (interrupt on abort, the run task on completion).
    run: Mutex<Option<JoinHandle<()>>>,
    /// Set by [`SessionHandle::interrupt`](Self::interrupt) before the
    /// abort lands, so the completing run knows a TurnAborted was already
    /// published on its behalf.
    aborted: AtomicBool,
    /// request_id -> decision channel; the engine parks on the receiver
    /// inside its approver (`block_in_place`, the TUI's pattern).
    pending: Mutex<HashMap<String, std::sync::mpsc::Sender<bool>>>,
    /// Sticky approve-rest (the TUI's `a` answer): every later mutating
    /// call in this session auto-approves.
    allow_rest: AtomicBool,
    /// Canonical grouped tool view for Snapshot `live`.
    tracker: Mutex<ActivityTracker>,
    /// In-flight streaming buffers (cleared on the `Done` delta).
    text: Mutex<String>,
    reasoning: Mutex<String>,
    /// The approval awaiting a decision (Snapshot `live.pending_approval`).
    live_approval: Mutex<Option<PendingApproval>>,
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
}

impl SessionHandle {
    /// Locked access to the session's engine.
    pub fn engine_cell(&self) -> MutexGuard<'_, Engine> {
        lock(&self.engine)
    }

    /// Assign the next seq and broadcast. The seq lock spans the send so
    /// concurrent publishers cannot reorder frames.
    pub fn publish(&self, payload: Payload) {
        let mut seq = lock(&self.seq);
        *seq += 1;
        let frame = Frame::event(Some(self.id), *seq, payload);
        let _ = self.tx.send(frame); // Err = no attached clients
    }

    /// The seq watermark a Snapshot should carry.
    pub fn current_seq(&self) -> u64 {
        *lock(&self.seq)
    }

    /// Subscribe to this session's frame stream (attach step 1: subscribe
    /// BEFORE loading the snapshot so no live frame is missed).
    pub fn subscribe(&self) -> broadcast::Receiver<Frame> {
        self.tx.subscribe()
    }

    /// Busy = a run task is parked in the slot.
    pub fn is_busy(&self) -> bool {
        lock(&self.run).is_some()
    }

    /// Whether the sticky approve-rest flag is armed.
    pub fn allow_rest_armed(&self) -> bool {
        self.allow_rest.load(Ordering::Relaxed)
    }

    /// The working dir this session's engine runs in (worktree tasks run
    /// inside their worktree).
    pub fn working_dir(&self) -> PathBuf {
        lock(&self.engine).config.working_dir.clone()
    }

    /// The live (non-persisted) half of a snapshot.
    pub fn live_state(&self) -> LiveState {
        let tracker = lock(&self.tracker);
        LiveState {
            running: self.is_busy(),
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
                })
                .collect(),
            pending_approval: lock(&self.live_approval).clone(),
            cumulative_usage: *lock(&self.usage),
            model: lock(&self.model).clone(),
            provider: lock(&self.provider).clone(),
            goal: lock(&self.goal).clone(),
        }
    }

    /// Delta sink: convert to the wire form, buffer the streaming state,
    /// clear buffers on `Done` (the per-turn boundary), publish.
    pub fn on_stream_delta(&self, delta: StreamDelta) {
        match WireDelta::from_core(&delta) {
            Some(WireDelta::Text { text }) => {
                lock(&self.text).push_str(&text);
                self.publish(Payload::Delta {
                    delta: WireDelta::Text { text },
                });
            }
            Some(WireDelta::Reasoning { text }) => {
                lock(&self.reasoning).push_str(&text);
                self.publish(Payload::Delta {
                    delta: WireDelta::Reasoning { text },
                });
            }
            Some(WireDelta::Done) => {
                *lock(&self.text) = String::new();
                *lock(&self.reasoning) = String::new();
                lock(&self.tracker).clear_turn();
                self.publish(Payload::Delta {
                    delta: WireDelta::Done,
                });
            }
            // `ToolCallReady` never reaches sinks (engine intercepts it).
            None => {}
        }
    }

    /// Tool-activity sink: feed the session's tracker, publish.
    pub fn on_tool_activity(&self, activity: ToolActivity) {
        lock(&self.tracker).on_tool(&activity);
        self.publish(Payload::Tool {
            activity: WireToolActivity::from(&activity),
        });
    }

    /// The session approver (Ask mode). Sticky allow-rest short-circuits;
    /// otherwise register the request, broadcast it to every attached
    /// client, and park the engine task on a sync channel until a client
    /// answers (or an interrupt drains the pending map with denials, or
    /// [`APPROVAL_TIMEOUT`](Self::APPROVAL_TIMEOUT) elapses — an
    /// unanswered request can never wedge the session).
    pub fn approve_gate(&self, call: &ToolCall) -> bool {
        if self.allow_rest.load(Ordering::Relaxed) {
            return true;
        }
        const APPROVAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
        let request_id = format!("req-{}", Uuid::new_v4());
        let (respond, receive) = std::sync::mpsc::channel::<bool>();
        lock(&self.pending).insert(request_id.clone(), respond);
        *lock(&self.live_approval) = Some(PendingApproval {
            request_id: request_id.clone(),
            call: call.clone(),
        });
        self.publish(Payload::ApprovalRequested {
            request_id: request_id.clone(),
            call: call.clone(),
        });
        // The whole round-trip is blocking inside an async context:
        // block_in_place frees the runtime's other workers while this task
        // waits for a decision.
        let allowed =
            tokio::task::block_in_place(|| receive.recv_timeout(APPROVAL_TIMEOUT).unwrap_or(false));
        lock(&self.pending).remove(&request_id);
        *lock(&self.live_approval) = None;
        allowed
    }

    /// Apply a client's decision (first responder wins). `AllowAll` also
    /// sets the sticky allow-rest flag. Broadcasts the resolution so every
    /// attached client sees it.
    pub fn resolve(&self, request_id: &str, decision: Decision) -> Result<(), String> {
        let respond = lock(&self.pending)
            .remove(request_id)
            .ok_or_else(|| format!("unknown or already-resolved request id {request_id}"))?;
        if decision == Decision::AllowAll {
            self.allow_rest.store(true, Ordering::Relaxed);
        }
        *lock(&self.live_approval) = None;
        let _ = respond.send(decision != Decision::Deny);
        self.publish(Payload::ApprovalResolved {
            request_id: request_id.to_string(),
            decision,
        });
        Ok(())
    }

    /// Abort the active run. Publishes `TurnAborted` exactly once (the
    /// aborted task's own completion path is suppressed via the slot), and
    /// denies every pending approval so a parked approver always releases
    /// — even in the degenerate no-run case, which must not leave a
    /// blocking approver wedged. Returns whether a run was aborted.
    pub fn interrupt(&self) -> bool {
        let task = lock(&self.run).take();
        let had_run = task.is_some();
        if had_run {
            self.aborted.store(true, Ordering::Relaxed);
            if let Some(task) = task {
                task.abort();
            }
        }
        for (_, respond) in lock(&self.pending).drain() {
            let _ = respond.send(false);
        }
        *lock(&self.live_approval) = None;
        if had_run {
            self.publish(Payload::TurnAborted {});
        }
        had_run
    }

    /// The run task calls this after its engine future settles. Whoever
    /// successfully takes the run slot owns the terminal frame; if
    /// [`interrupt`](Self::interrupt) already took it, TurnAborted was
    /// published there and this is a no-op.
    pub fn complete(&self, result: modron_core::Result<RunSummary>) {
        let mut slot = lock(&self.run);
        if slot.take().is_none() {
            return; // interrupted: the slot (and the terminal frame) went to it
        }
        drop(slot);
        match result {
            Ok(summary) => self.publish(Payload::TurnDone {
                summary: WireRunSummary::from(&summary),
            }),
            Err(error) => self.publish(Payload::TurnFailed {
                error: error.to_string(),
            }),
        }
    }

    /// Fold one persisted event into the live view (usage accounting) and
    /// broadcast it as a seq'd `Event` frame.
    pub fn on_persisted(&self, event: &SessionEvent) {
        if let SessionEvent::Usage { usage, .. } = event {
            let mut total = lock(&self.usage);
            total.input_tokens += usage.input_tokens;
            total.output_tokens += usage.output_tokens;
            total.cached_input_tokens += usage.cached_input_tokens;
        }
        self.publish(Payload::Event {
            event: event.clone(),
        });
    }

    /// Mutate the session's engine config for future turns (the wire's
    /// `SetConfig`). The model/provider switch follows the TUI's /model:
    /// provider follows the catalog (or the explicit patch), and the
    /// selector is pinned so the router cannot route elsewhere. Returns a
    /// human note (published as `ConfigChanged` by the caller).
    pub fn apply_config(&self, patch: &ConfigPatch) -> anyhow::Result<()> {
        let mut engine = self.engine_cell().clone();
        let mut config = engine.config.clone();

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
            *lock(&self.model) = model.clone();
            *lock(&self.provider) = config.default_provider.clone();
        } else if let Some(provider) = &patch.provider {
            config.default_provider = provider.clone();
            *lock(&self.provider) = provider.clone();
        }
        if let Some(sandbox) = patch.sandbox {
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
        if let Some(allow) = &patch.allow {
            config.approvals.allow = allow.clone();
        }
        if let Some(deny) = &patch.deny {
            config.approvals.deny = deny.clone();
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
                *lock(&self.model) = model;
            }
            if let Some(provider) = spec.provider {
                config.default_provider = provider.clone();
                *lock(&self.provider) = provider;
            }
            config.approvals.allow.extend(spec.allow);
            config.approvals.deny.extend(spec.deny);
        }
        if let Some(goal) = &patch.goal {
            *lock(&self.goal) = goal.clone();
        }

        engine.config = config;
        *self.engine_cell() = engine;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// SessionHub — the map of live sessions
// ---------------------------------------------------------------------------

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
            }),
        }
    }

    /// Install the template engine. Its store must already be the hub's
    /// [`BroadcastingStore`] so every session's appends fan out.
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

    /// Live session ids (tests, admin views).
    pub fn session_ids(&self) -> Vec<SessionId> {
        lock(&self.inner.sessions).keys().copied().collect()
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
        let template = lock(&self.inner.template)
            .clone()
            .expect("SessionHub::set_engine must run before sessions are created");
        let shared_store = Arc::clone(&template.store);
        let mut engine = template;
        engine.store = PinnedCreateStore::new(shared_store, pinned);

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
        config.approver = Some(Arc::new(move |call: &ToolCall| {
            approval_hub
                .handle(&id)
                .map(|handle| handle.approve_gate(call))
                .unwrap_or(false)
        }));
        if let Some(dir) = working_dir {
            config.working_dir = dir;
        }
        engine.config = config;

        let (tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        let handle = Arc::new(SessionHandle {
            id,
            tx,
            seq: Mutex::new(0),
            run: Mutex::new(None),
            aborted: AtomicBool::new(false),
            pending: Mutex::new(HashMap::new()),
            allow_rest: AtomicBool::new(false),
            tracker: Mutex::new(ActivityTracker::new()),
            text: Mutex::new(String::new()),
            reasoning: Mutex::new(String::new()),
            live_approval: Mutex::new(None),
            usage: Mutex::new(Usage::default()),
            model: Mutex::new(engine.config.default_model.clone()),
            provider: Mutex::new(engine.config.default_provider.clone()),
            goal: Mutex::new(String::new()),
            engine: Mutex::new(engine),
        });
        sessions.insert(id, Arc::clone(&handle));
        handle
    }

    /// One persisted event reached the shared store: fold it into the
    /// session's live view and fan it out. Unknown sessions (e.g. events
    /// appended outside the server) are ignored.
    pub fn on_persisted(&self, id: SessionId, event: SessionEvent) {
        if let Some(handle) = self.handle(&id) {
            handle.on_persisted(&event);
        }
    }

    /// Spawn a run for a session: busy-check + slot store are atomic, the
    /// run task itself publishes the terminal frame on completion.
    pub fn start_run(&self, id: SessionId, run: RunFuture) -> Result<(), String> {
        let handle = self
            .handle(&id)
            .ok_or_else(|| format!("no such session {id}"))?;
        let mut slot = lock(&handle.run);
        if slot.is_some() {
            return Err("session busy (a run is active)".to_string());
        }
        let owner = Arc::clone(&handle);
        let task = tokio::spawn(async move {
            let result = run.await;
            owner.complete(result);
        });
        *slot = Some(task);
        Ok(())
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
