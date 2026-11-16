//! The live session driver, shared by both hosts. A [`Driver`] owns one
//! `RemoteClient`; each [`Driver::step`] applies the UI's pending actions,
//! runs the post-connect setup (attach, or pick a session), retries a dropped
//! connection, and publishes the snapshot when it changed. The view side is
//! [`Live`], which moves those snapshots into the host and the host's actions
//! back to the driver.
//!
//! Who calls `step`: the terminal host runs it on its own thread
//! ([`spawn`]); the web host runs it from the page's timer. The core needs no
//! runtime of its own on either target (native owns one; wasm uses
//! `spawn_local`), so the driver itself is plain sync code.
//!
//! Connection loss: the driver keeps the core's `disconnected` state on
//! screen, retries every 2 s (doubling, capped at 30 s), and re-attaches to
//! the session it had. [`Status::reconnecting`] tells the terminal host that
//! Esc may leave.

use modron_mobile_core::view::{
    connection_address_label, no_session_to_continue, NO_SESSION_LABEL,
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use modron_mobile_core::{
    approval_mode_next, connection_label, sandbox_next, ClientError, ConfigPatch, ConnectOptions,
    ConnectionState, Phase, RemoteClient, ViewModelSnapshot, PLAN_DECISION_LABELS,
};
use modron_protocol::SessionMetaWire;
use tokio::sync::mpsc::{
    error::TryRecvError, unbounded_channel, UnboundedReceiver, UnboundedSender,
};
use uuid::Uuid;

use crate::chat::{Host, UiAction};
use crate::clock::Instant;

/// How often the driver is stepped.
pub const TICK: Duration = Duration::from_millis(20);

/// How long a session list may take to arrive. The rows land in the state's
/// `sessions`, and an empty list looks the same as a late one, so the wait
/// is bounded.
pub const LIST_WAIT: Duration = Duration::from_secs(2);

/// How long a connection may stay `Connecting` before the driver tries
/// again (with the reconnect backoff). The core's connect is fire-and-forget,
/// so a silent failure would otherwise leave the header on "connecting".
pub const CONNECT_STALL: Duration = Duration::from_secs(3);

/// Debug trace for the driver (T1, FINDINGS F14): appends one line per call
/// to the file named by `MODRON_APP_TRACE`. Off unless the variable is set.
pub fn trace(line: impl AsRef<str>) {
    use std::io::Write;
    let Some(path) = std::env::var_os("MODRON_APP_TRACE") else {
        return;
    };
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let _ = writeln!(file, "{stamp} {}", line.as_ref());
    }
}

/// Whether a connect started at `since` has stalled by `now`.
pub fn connect_stalled(since: Instant, now: Instant) -> bool {
    now >= since + CONNECT_STALL
}

/// The first reconnect delay; each failed try doubles it up to the cap.
pub const RETRY_FIRST: Duration = Duration::from_secs(2);
/// The longest wait between reconnect tries.
pub const RETRY_MAX: Duration = Duration::from_secs(30);

/// Which session a connection attaches to when no exact one is given.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Resume {
    /// Nothing attached: the first submit starts a session.
    #[default]
    New,
    /// The most recently updated session whose folder is `cwd`, else new.
    Latest,
    /// The most recently updated session of any folder; none is an error.
    Continue,
}

/// Where the live driver connects, and which session it attaches to.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct LiveOptions {
    pub url: String,
    pub token: Option<String>,
    /// An exact session to attach to (wins over `resume`).
    pub session: Option<Uuid>,
    /// What to attach to when `session` is None.
    pub resume: Resume,
    /// The folder `Resume::Latest` matches against (the session's
    /// `working_dir`).
    pub cwd: Option<String>,
    /// The LAN `ws://` opt-in (`--insecure-lan`).
    pub insecure_lan: bool,
    /// The config the startup flags ask for, sent in order once a session is
    /// attached (and again after a reconnect), so it applies to future turns.
    pub startup: Vec<ConfigPatch>,
}

impl LiveOptions {
    /// Parses `--url <ws url> [--token <t>] [--session <uuid>]`. `Ok(None)`
    /// when no `--url` is given: the app then runs its demo script.
    pub fn from_args(args: impl IntoIterator<Item = String>) -> Result<Option<Self>, String> {
        let mut args = args.into_iter();
        let (mut url, mut token, mut session) = (None, None, None);
        while let Some(flag) = args.next() {
            let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
            match flag.as_str() {
                "--url" => url = Some(value),
                "--token" => token = Some(value),
                "--session" => {
                    session = Some(Uuid::parse_str(&value).map_err(|e| format!("--session: {e}"))?)
                }
                other => return Err(format!("unknown argument {other}")),
            }
        }
        match url {
            Some(url) => Ok(Some(Self { url, token, session, ..Self::default() })),
            None if token.is_some() || session.is_some() => {
                Err("--token and --session need --url".into())
            }
            None => Ok(None),
        }
    }

    /// Parses a page address's query, `?url=<ws url>&token=<t>&session=<uuid>`
    /// (form encoding, as `URLSearchParams` reads it). `Ok(None)` when there is
    /// no `url` value: the web host then runs the demo. Unknown keys are ignored.
    pub fn from_query(query: &str) -> Result<Option<Self>, String> {
        let (mut url, mut token, mut session) = (None, None, None);
        for pair in query.trim_start_matches('?').split('&').filter(|p| !p.is_empty()) {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            let value = decode_form(value);
            match key {
                "url" => url = Some(value),
                "token" => token = Some(value).filter(|t| !t.is_empty()),
                "session" => {
                    session = Some(Uuid::parse_str(&value).map_err(|e| format!("session: {e}"))?)
                }
                _ => {}
            }
        }
        Ok(url.filter(|u| !u.is_empty()).map(|url| Self { url, token, session, ..Self::default() }))
    }
}

/// Decodes one form-encoded query value: `+` is a space, `%XX` a byte. Bad
/// escapes are kept as written.
fn decode_form(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                match std::str::from_utf8(&bytes[i + 1..i + 3])
                    .ok()
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                {
                    Some(byte) => {
                        out.push(byte);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            byte => out.push(byte),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Flags the driver raises for the terminal host, shared between the two.
#[derive(Debug, Default)]
pub struct Status {
    reconnecting: AtomicBool,
    fatal: Mutex<Option<String>>,
    note: Mutex<String>,
}

impl Status {
    /// True while the connection is down and the driver is retrying.
    pub fn reconnecting(&self) -> bool {
        self.reconnecting.load(Ordering::SeqCst)
    }

    /// The reason the driver stopped on its own (for example `Resume::Continue`
    /// with no session to continue). The host leaves its screen when set.
    pub fn fatal(&self) -> Option<String> {
        self.fatal.lock().ok().and_then(|f| f.clone())
    }

    /// Text appended to the connection label, e.g. who started the server.
    pub fn set_note(&self, text: &str) {
        if let Ok(mut note) = self.note.lock() {
            *note = text.into();
        }
    }

    fn note(&self) -> String {
        self.note.lock().map(|n| n.clone()).unwrap_or_default()
    }

    fn fail(&self, reason: String) {
        if let Ok(mut fatal) = self.fatal.lock() {
            *fatal = Some(reason);
        }
    }
}

/// The view's end of the driver.
pub struct Live {
    snapshots: UnboundedReceiver<ViewModelSnapshot>,
    actions: UnboundedSender<UiAction>,
    /// The driver's flags (shared with the driver thread).
    pub status: Arc<Status>,
}

impl Live {
    /// Creates the link: the view side, the driver's action receiver and the
    /// driver's snapshot sender.
    pub fn channels() -> (Self, UnboundedReceiver<UiAction>, UnboundedSender<ViewModelSnapshot>) {
        let (snap_tx, snap_rx) = unbounded_channel();
        let (act_tx, act_rx) = unbounded_channel();
        let live = Self { snapshots: snap_rx, actions: act_tx, status: Arc::default() };
        (live, act_rx, snap_tx)
    }

    /// Moves the driver's snapshots into the host and the host's actions to
    /// the driver. Call it from the UI loop. Returns true when a snapshot
    /// arrived, so the caller redraws.
    pub fn pump(&mut self, host: &mut Host) -> bool {
        let mut changed = false;
        while let Ok(snapshot) = self.snapshots.try_recv() {
            trace(format!("pump snapshot label={:?}", snapshot.connection_label));
            host.push(&snapshot);
            changed = true;
        }
        while let Some(action) = host.try_action() {
            let _ = self.actions.send(action);
        }
        changed
    }

    /// Sends one action to the driver directly (no host in between).
    pub fn act(&self, action: UiAction) {
        let _ = self.actions.send(action);
    }

    /// The next snapshot the driver published, if one is waiting.
    pub fn try_snapshot(&mut self) -> Option<ViewModelSnapshot> {
        self.snapshots.try_recv().ok()
    }
}

/// Starts the driver on its own thread (terminal host). Connecting is
/// asynchronous: the first snapshot shows `connecting`.
#[cfg(not(target_arch = "wasm32"))]
pub fn spawn(options: LiveOptions) -> std::io::Result<Live> {
    let (live, actions, out) = Live::channels();
    let status = Arc::clone(&live.status);
    std::thread::Builder::new().name("modron-session".into()).spawn(move || {
        if let Some(mut driver) = Driver::start(options, status, actions, out) {
            while driver.step() {
                std::thread::sleep(TICK);
            }
        }
    })?;
    Ok(live)
}

/// Picks the session a connection resumes: the most recently updated row
/// (`updated_at` is RFC 3339 from the server, so text order is time order),
/// among the rows in `cwd` for [`Resume::Latest`]. `None` for `New`.
pub fn pick_session(rows: &[SessionMetaWire], resume: Resume, cwd: Option<&str>) -> Option<Uuid> {
    let folder = match resume {
        Resume::New => return None,
        Resume::Latest => cwd,
        Resume::Continue => None,
    };
    rows.iter()
        .filter(|row| folder.is_none_or(|dir| row.working_dir.as_deref() == Some(dir)))
        .max_by(|a, b| a.updated_at.cmp(&b.updated_at))
        .map(|row| row.id)
}

/// How long to wait before reconnect try number `attempts` (0-based).
pub fn retry_delay(attempts: u32) -> Duration {
    let factor = 1u32.checked_shl(attempts.min(16)).unwrap_or(u32::MAX);
    RETRY_FIRST.saturating_mul(factor).min(RETRY_MAX)
}

/// Where the driver is in the session choice after a connect.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Stage {
    Ready,
    /// A session list was requested; choose by this deadline at the latest.
    Listing(Instant),
}

/// The driver: one client, its connection and attach state, and the last
/// snapshot sent.
pub struct Driver {
    client: RemoteClient,
    options: LiveOptions,
    status: Arc<Status>,
    actions: UnboundedReceiver<UiAction>,
    out: UnboundedSender<ViewModelSnapshot>,
    /// The current connection ran its post-connect setup.
    linked: bool,
    stage: Stage,
    /// The session last seen attached: a reconnect re-attaches to it.
    resume: Option<Uuid>,
    /// When the next reconnect try runs, and how many have run.
    retry_at: Option<Instant>,
    /// When the current `Connecting` state began (None when not connecting).
    connecting_since: Option<Instant>,
    /// The last connection label written to the trace (debug only).
    traced: String,
    attempts: u32,
    last: String,
    /// The staged chip labels the last snapshot carried. The chips are read
    /// into the view at publish time, so a change to them publishes too.
    staged: Vec<String>,
    /// The startup config went out for the current connection.
    startup_sent: bool,
}

impl Driver {
    /// Starts the client. `None` when the URL is refused: the failure is sent
    /// as a `Disconnected` snapshot first, so the view still shows it.
    pub fn start(
        options: LiveOptions,
        status: Arc<Status>,
        actions: UnboundedReceiver<UiAction>,
        out: UnboundedSender<ViewModelSnapshot>,
    ) -> Option<Self> {
        trace(format!("driver start url={}", options.url));
        match RemoteClient::connect_with_options(
            &options.url,
            options.token.as_deref(),
            connect_options(&options),
        ) {
            Ok(client) => Some(Self {
                traced: String::new(),
                client,
                options,
                status,
                actions,
                out,
                linked: false,
                stage: Stage::Ready,
                resume: None,
                retry_at: None,
                connecting_since: None,
                attempts: 0,
                last: String::new(),
                staged: Vec::new(),
                startup_sent: false,
            }),
            Err(e) => {
                let _ = out.send(ViewModelSnapshot {
                    connection: ConnectionState::Disconnected,
                    connection_label: connection_label(&ConnectionState::Disconnected).into(),
                    last_error: Some(e.to_string()),
                    ..Default::default()
                });
                None
            }
        }
    }

    /// One pass: apply the UI's actions, keep the connection up, publish the
    /// snapshot if it changed. Returns false when the driver is finished: the
    /// view is gone, the driver hit a fatal condition (see [`Status::fatal`]),
    /// or the snapshot receiver is gone. A dropped connection is not finished:
    /// the driver retries it.
    pub fn step(&mut self) -> bool {
        loop {
            match self.actions.try_recv() {
                Ok(action) => apply(&self.client, action),
                Err(TryRecvError::Empty) => break,
                // The view is gone: nothing left to drive.
                Err(TryRecvError::Disconnected) => return false,
            }
        }
        let state = self.client.state();
        if self.traced != state.connection_label {
            trace(format!(
                "state {:?} label={:?} error={:?}",
                state.connection, state.connection_label, state.last_error
            ));
            self.traced = state.connection_label.clone();
        }
        if state.session_id.is_some() {
            self.resume = state.session_id;
        }
        match state.connection {
            ConnectionState::Connected => {
                self.status.reconnecting.store(false, Ordering::SeqCst);
                self.connecting_since = None;
                if !self.linked {
                    self.linked = true;
                    self.attempts = 0;
                    self.retry_at = None;
                    self.link_up();
                    // The credentials rows, for the first-run check (Nav::first_run),
                    // and the session list, whatever the resume mode (the
                    // Sessions screen shows it; a live page with no session
                    // parameter would otherwise never ask).
                    let _ = self.client.auth_providers();
                    let _ = self.client.list_sessions();
                }
            }
            ConnectionState::Connecting => {
                let since = *self.connecting_since.get_or_insert_with(Instant::now);
                if connect_stalled(since, Instant::now()) {
                    self.connecting_since = None;
                    self.attempts = self.attempts.saturating_add(1);
                    self.reconnect();
                }
            }
            ConnectionState::Disconnected => {
                self.linked = false;
                self.startup_sent = false;
                self.status.reconnecting.store(true, Ordering::SeqCst);
                let due = *self
                    .retry_at
                    .get_or_insert_with(|| Instant::now() + retry_delay(self.attempts));
                if Instant::now() >= due {
                    self.retry_at = None;
                    self.attempts = self.attempts.saturating_add(1);
                    self.reconnect();
                }
            }
        }
        if let Stage::Listing(deadline) = self.stage {
            if !state.sessions.is_empty() || Instant::now() >= deadline {
                self.stage = Stage::Ready;
                match pick_session(
                    &state.sessions,
                    self.options.resume,
                    self.options.cwd.as_deref(),
                ) {
                    Some(session) => {
                        let _ = self.client.attach(session, 0);
                    }
                    None if self.options.resume == Resume::Continue => {
                        self.status.fail(no_session_to_continue(&self.options.url));
                        return false;
                    }
                    None if !self.options.startup.is_empty() => {
                        let _ = self.client.new_session(None);
                    }
                    None => {}
                }
            }
        }
        // The startup config waits for a session: SetConfig needs one attached.
        if !self.startup_sent && state.session_id.is_some() {
            self.startup_sent = true;
            for patch in self.options.startup.clone() {
                if let Err(error) = self.client.set_config(patch) {
                    self.client.note_local_error(error.to_string());
                }
            }
        }
        let mut snapshot = self.client.state();
        snapshot.connection_label = self.label(&snapshot.connection_label);
        // Snapshots carry no PartialEq; the JSON form is the change test.
        let json = serde_json::to_string(&snapshot).unwrap_or_default();
        // A submit that clears the staged image chips changes nothing in the
        // core, so the chips are compared here too (the view copies them).
        let staged = crate::chat::staged_labels();
        if publish_due(&json, &self.last, &staged, &self.staged) {
            self.last = json;
            self.staged = staged;
            if self.out.send(snapshot).is_err() {
                return false;
            }
        }
        true
    }

    /// The connection label with the address (and the note, when set).
    fn label(&self, base: &str) -> String {
        let note = self.status.note();
        connection_address_label(base, &self.options.url, &note)
    }

    /// The post-connect setup: an exact session is attached; with none, the
    /// resume choice runs. A reconnect that already names a session has
    /// re-attached itself (the core does it after the welcome).
    fn link_up(&mut self) {
        if self.resume.is_some() {
            return;
        }
        if let Some(session) = self.options.session {
            if self.client.attached_session() != Some(session) {
                let _ = self.client.attach(session, 0);
            }
            return;
        }
        if self.options.resume != Resume::New {
            let _ = self.client.list_sessions();
            self.stage = Stage::Listing(Instant::now() + LIST_WAIT);
        } else if !self.options.startup.is_empty() {
            // Startup flags need a session to configure: start one now.
            let _ = self.client.new_session(None);
        }
    }

    /// One reconnect try: re-attach to the known session, or connect afresh
    /// when no session was ever seen (the post-connect setup runs again).
    fn reconnect(&mut self) {
        trace(format!("reconnect resume={:?} attempts={}", self.resume, self.attempts));
        let token = self.options.token.as_deref();
        match self.resume {
            Some(session) => {
                let _ = self.client.reconnect_with_options(
                    &self.options.url,
                    session,
                    token,
                    self.options.insecure_lan,
                );
            }
            None => {
                if let Ok(client) = RemoteClient::connect_with_options(
                    &self.options.url,
                    token,
                    connect_options(&self.options),
                ) {
                    self.client = client;
                }
            }
        }
        self.stage = Stage::Ready;
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        self.client.disconnect();
    }
}

/// Whether the driver publishes a snapshot: the core state changed, or the
/// staged chips changed (a submit that clears them changes no core state).
fn publish_due(json: &str, last: &str, staged: &[String], last_staged: &[String]) -> bool {
    json != last || staged != last_staged
}

/// Maps one UI action to its core call. A call the core refuses (no session
/// attached, a closed link) is shown as the snapshot's error, so the user
/// sees why nothing happened. Server refusals arrive as `Payload::Error` and
/// land in the same field.
fn apply(client: &RemoteClient, action: UiAction) {
    let result = match action {
        UiAction::Send(text) => {
            // The staged images go with this submit; they are cleared only
            // when the core accepts it (a refusal keeps the chips).
            let staged = crate::chat::staged_wire();
            let had_images = !staged.is_empty();
            let result = client.send_message_with(text, staged);
            if result.is_ok() && had_images {
                crate::chat::clear_staged();
                // The "attached ..." notice referred to this message.
                client.clear_local_error();
            }
            result
        }
        UiAction::Detach => {
            if crate::chat::unstage_last() {
                client.note_local_error(ATTACH_REMOVED_NOTE);
            }
            Ok(())
        }
        UiAction::Paste => {
            // Clipboard text is offered to the composer (no note); an empty
            // note means nothing to show.
            let note = paste_clipboard();
            if !note.is_empty() {
                client.note_local_error(note);
            }
            Ok(())
        }
        UiAction::Queue(text) => client.enqueue(text),
        UiAction::Dequeue(index) => client.dequeue(u32::try_from(index).unwrap_or(u32::MAX)),
        UiAction::Decide(index) => {
            let snapshot = client.state();
            let answer = snapshot
                .pending_approval
                .as_ref()
                .and_then(|p| p.options.get(index).map(|o| (p.request_id.clone(), o.decision)));
            match answer {
                Some((request_id, decision)) => client.approve(request_id, decision),
                None => Ok(()),
            }
        }
        UiAction::DecidePlan { approve, note } => {
            // PLAN_DECISION_LABELS is in protocol order: approve, then revise.
            let index = if approve { 0 } else { 1 };
            client.decide_plan(PLAN_DECISION_LABELS[index].0, note)
        }
        UiAction::Interrupt => match client.state().session_id {
            Some(session) => client.interrupt(session),
            None => Ok(()),
        },
        UiAction::ToggleReasoning => {
            // The display follows the local toggle at once. The provider's
            // reasoning summary follows it too (the core asks the server); a
            // refusal (a portal tunnel's non-owner) shows as the error line.
            let shown = !client.state().show_reasoning;
            client.set_show_reasoning(shown);
            client.set_reasoning_summary(shown)
        }
        UiAction::Compact => client.compact(),
        UiAction::Shell { command, private: false } => client.shell(command),
        UiAction::Shell { command, private: true } => client.shell_private(command),
        UiAction::ListSessions => client.list_sessions(),
        UiAction::Attach(session) => client.attach(session, 0),
        UiAction::NewSession => client.new_session(None),
        UiAction::SetApprovalMode(mode) => {
            client.set_config(ConfigPatch { approval_mode: Some(mode), ..Default::default() })
        }
        UiAction::SetPhase(phase) => client.set_phase(phase),
        UiAction::SetEffort(effort) => client.set_effort(effort),
        UiAction::SetSandbox(policy) => {
            client.set_config(ConfigPatch { sandbox: Some(policy), ..Default::default() })
        }
        UiAction::SelectModel(model) => client.select_model(model),
        UiAction::SaveDefaultModel { model, provider } => {
            client.persist_default_model(&model, &provider)
        }
        // The catalog (`models_req`) and the router defaults: the picker
        // shows the catalog once it arrives and the defaults until then.
        UiAction::ListModels => client.request_models().and_then(|()| client.router_defaults()),
        UiAction::CycleMode => {
            let mode = approval_mode_next(client.state().approval_mode);
            client.set_config(ConfigPatch { approval_mode: Some(mode), ..Default::default() })
        }
        UiAction::CycleSandbox => {
            let policy = sandbox_next(client.state().sandbox_policy);
            client.set_config(ConfigPatch { sandbox: Some(policy), ..Default::default() })
        }
        UiAction::CreateTask(title) => client.create_task(&title, None),
        UiAction::AddToolRule { allow, tool } => {
            let state = client.state();
            let mut list = if allow { state.allow_tools.clone() } else { state.deny_tools.clone() };
            if !list.contains(&tool) {
                list.push(tool);
            }
            if allow {
                client.set_tool_lists(Some(list), None)
            } else {
                client.set_tool_lists(None, Some(list))
            }
        }
        UiAction::ShowToolLists => {
            let state = client.state();
            let note = format!(
                "allow: {} | deny: {}",
                tool_list_text(&state.allow_tools),
                tool_list_text(&state.deny_tools)
            );
            client.note_local_error(note);
            Ok(())
        }
        UiAction::Fork => with_session(client, |s| client.fork_session(s)),
        UiAction::TaskStatus => with_session(client, |s| client.task_status(s)),
        UiAction::Share => with_session(client, |s| {
            let content = client
                .state()
                .history
                .iter()
                .map(|row| row.lines.join("\n"))
                .collect::<Vec<_>>()
                .join("\n\n");
            client.publish_page(s, SHARE_TITLE, content)
        }),
        UiAction::RemoteControl(url) => with_session(client, |s| client.remote_control(s, &url)),
        UiAction::DeleteSession(id) => client.delete_session(id),
        UiAction::SteerAgent { agent, text } => client.steer_agent(agent, text),
        UiAction::KillAgent(agent) => client.kill_agent(agent),
        UiAction::ListAuth => client.auth_providers(),
        UiAction::ListPersonas => client.request_personas(),
        UiAction::RefreshTaskStatuses => client.refresh_task_statuses().map(|_| ()),
        UiAction::AuthRemove(provider) => {
            with_session(client, |s| client.auth_remove(s, &provider))
        }
        UiAction::Plan(task) => match client.set_phase(Phase::Plan) {
            Ok(()) => client.send_message(task),
            Err(error) => Err(error),
        },
        UiAction::SetGoal(goal) => client.set_goal(goal),
        UiAction::SetPersona(name) => client.set_persona(name),
        // The host sets the quit flag and never forwards it.
        UiAction::Exit => Ok(()),
        UiAction::AuthStart(provider) => with_session(client, |s| client.auth_start(s, &provider)),
        UiAction::SetKey(secret) => {
            with_session(client, |s| client.auth_set_key(s, &secret.provider, &secret.key))
        }
        UiAction::ListAutomations => client.list_automations(),
        UiAction::AddAutomation { name, prompt, interval_minutes } => {
            client.add_automation(name, prompt, interval_minutes)
        }
        UiAction::RemoveAutomation(name) => client.remove_automation(name),
        UiAction::TaskStatusOf(session) => client.task_status(session),
        // The rules setter replaces the whole set, so the rest is sent.
        UiAction::RemoveRule(index) => {
            match modron_mobile_core::view::rules_without(&client.state().rules, index) {
                Some(rules) => {
                    client.set_config(ConfigPatch { rules: Some(rules), ..Default::default() })
                }
                None => Ok(()),
            }
        }
        UiAction::RemoveToolRule { allow, tool } => {
            let state = client.state();
            if allow {
                let list = modron_mobile_core::view::tools_without(&state.allow_tools, &tool);
                client.set_tool_lists(Some(list), None)
            } else {
                let list = modron_mobile_core::view::tools_without(&state.deny_tools, &tool);
                client.set_tool_lists(None, Some(list))
            }
        }
        UiAction::MergeTask(session) => client.merge_task(session, None),
        UiAction::AttachFile { name, mime, bytes } => {
            // The same chip as Ctrl+V; a refusal from the core's type check is
            // the note (the chip is not staged).
            match modron_mobile_core::attachment_from_bytes(&name, &mime, &bytes) {
                Ok(wire) => {
                    let size = bytes.len() as u64;
                    let label = crate::chat::chip_label(&name, size);
                    crate::chat::stage(crate::chat::Staged { name, bytes: size, wire });
                    client.note_local_error(format!(
                        "attached {label} (goes with your next message)"
                    ));
                }
                Err(reason) => client.note_local_error(reason),
            }
            Ok(())
        }
        UiAction::Notice(text) => {
            client.note_local_error(text);
            Ok(())
        }
        // Navigation is applied by the host, and the owner portal's actions
        // are the web host's (`Host::take_portal`); neither reaches here.
        UiAction::Nav(_)
        | UiAction::Tools(_)
        | UiAction::SignIn(_)
        | UiAction::SignOut
        | UiAction::RefreshPortal
        | UiAction::OpenUrl(_)
        | UiAction::ListInvites
        | UiAction::CreateInvite(_)
        | UiAction::RevokeInvite(_)
        | UiAction::CopyInvite(_)
        | UiAction::TogglePush
        | UiAction::Clear
        | UiAction::Context => Ok(()),
    };
    if let Err(error) = result {
        client.note_local_error(error.to_string());
    }
}

/// The core's connect options for these live options (the LAN opt-in).
fn connect_options(options: &LiveOptions) -> ConnectOptions {
    ConnectOptions { allow_insecure: options.insecure_lan, ..ConnectOptions::default() }
}

/// The title of a shared transcript page (`/share`).
const SHARE_TITLE: &str = "modron session";

/// Runs a call that needs the attached session. With none, the shared
/// no-session word is the snapshot's error (the call is refused locally).
fn with_session(
    client: &RemoteClient,
    call: impl FnOnce(Uuid) -> Result<(), ClientError>,
) -> Result<(), ClientError> {
    match client.state().session_id {
        Some(session) => call(session),
        None => {
            client.note_local_error(NO_SESSION_LABEL);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cleared_chip_publishes_even_when_the_core_state_is_unchanged() {
        let chip = vec!["[img shot.png \u{b7} 2.0 KB]".to_string()];
        assert!(!publish_due("{}", "{}", &chip, &chip));
        assert!(publish_due("{}", "{}", &[], &chip));
        assert!(publish_due("{}", "{}", &chip, &[]));
        assert!(publish_due("{\"a\":1}", "{}", &[], &[]));
    }

    #[test]
    fn query_without_url_is_the_demo() {
        assert_eq!(LiveOptions::from_query(""), Ok(None));
        assert_eq!(LiveOptions::from_query("?token=t"), Ok(None));
        assert_eq!(LiveOptions::from_query("?url="), Ok(None));
    }

    #[test]
    fn query_reads_url_token_and_session() {
        let id = Uuid::new_v4();
        let query = format!("?url=ws%3A%2F%2Flocalhost%3A7000&token=a+b%21&session={id}");
        assert_eq!(
            LiveOptions::from_query(&query),
            Ok(Some(LiveOptions {
                url: "ws://localhost:7000".into(),
                token: Some("a b!".into()),
                session: Some(id),
                ..LiveOptions::default()
            }))
        );
    }

    #[test]
    fn query_rejects_a_bad_session_and_keeps_bad_escapes() {
        assert!(LiveOptions::from_query("?url=ws://h&session=nope").is_err());
        assert_eq!(decode_form("100%"), "100%");
        assert_eq!(decode_form("%zz"), "%zz");
    }

    fn row(id: u128, updated: &str, dir: Option<&str>) -> SessionMetaWire {
        SessionMetaWire {
            id: Uuid::from_u128(id),
            title: format!("s{id}"),
            created_at: updated.into(),
            updated_at: updated.into(),
            message_count: 1,
            task_branch: None,
            task_state: None,
            working_dir: dir.map(String::from),
        }
    }

    #[test]
    fn latest_picks_the_newest_row_in_the_folder() {
        let rows = [
            row(1, "2026-11-14T10:00:00+00:00", Some("/work/a")),
            row(2, "2026-11-15T09:00:00+00:00", Some("/work/b")),
            row(3, "2026-11-15T08:00:00+00:00", Some("/work/a")),
        ];
        assert_eq!(pick_session(&rows, Resume::Latest, Some("/work/a")), Some(Uuid::from_u128(3)));
        assert_eq!(pick_session(&rows, Resume::Latest, Some("/work/c")), None);
    }

    #[test]
    fn continue_ignores_the_folder_and_new_picks_nothing() {
        let rows = [
            row(1, "2026-11-14T10:00:00+00:00", Some("/work/a")),
            row(2, "2026-11-15T09:00:00+00:00", Some("/work/b")),
        ];
        assert_eq!(
            pick_session(&rows, Resume::Continue, Some("/work/a")),
            Some(Uuid::from_u128(2))
        );
        assert_eq!(pick_session(&rows, Resume::Continue, None), Some(Uuid::from_u128(2)));
        assert_eq!(pick_session(&rows, Resume::New, None), None);
        assert_eq!(pick_session(&[], Resume::Continue, None), None);
    }

    #[test]
    fn retry_doubles_from_two_seconds_and_caps_at_thirty() {
        let secs: Vec<u64> = (0..7).map(|n| retry_delay(n).as_secs()).collect();
        assert_eq!(secs, vec![2, 4, 8, 16, 30, 30, 30]);
        assert_eq!(retry_delay(u32::MAX), RETRY_MAX);
    }
}

#[cfg(test)]
mod stall_tests {
    use super::*;

    #[test]
    fn a_connect_is_stalled_only_after_the_stall_window() {
        let since = Instant::now();
        assert!(!connect_stalled(
            since,
            since + Duration::from_millis(CONNECT_STALL.as_millis() as u64 - 1)
        ));
        assert!(connect_stalled(since, since + CONNECT_STALL));
    }
}

/// Shown when a clipboard image is removed with Backspace.
const ATTACH_REMOVED_NOTE: &str = "image removed";
/// The web host has no clipboard read: Ctrl+V there says so.
#[cfg(target_arch = "wasm32")]
const CLIPBOARD_TEXT_NOTE: &str = "clipboard has text, not an image: paste it with the terminal";
#[cfg(not(target_arch = "wasm32"))]
const CLIPBOARD_EMPTY_NOTE: &str = "clipboard is empty";

/// Ctrl+V: reads the clipboard and stages an image as a chip. Returns the
/// note the status line shows. Runs the OS clipboard tools (bounded at 2 s
/// each) on the driver's thread.
fn paste_clipboard() -> String {
    #[cfg(not(target_arch = "wasm32"))]
    {
        use crate::clipboard::{harvest, Harvest};
        match harvest() {
            Harvest::Image { name, mime, bytes } => {
                match modron_mobile_core::attachment_from_bytes(&name, mime, &bytes) {
                    Ok(wire) => {
                        let size = bytes.len() as u64;
                        let label = crate::chat::chip_label(&name, size);
                        crate::chat::stage(crate::chat::Staged { name, bytes: size, wire });
                        format!("attached {label} (goes with your next message)")
                    }
                    Err(reason) => reason,
                }
            }
            Harvest::Text(text) => {
                crate::chat::offer_text(text);
                String::new()
            }
            Harvest::Empty => CLIPBOARD_EMPTY_NOTE.to_string(),
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        CLIPBOARD_TEXT_NOTE.to_string()
    }
}

/// A tool list as the status line shows it: comma separated, or `none`.
fn tool_list_text(tools: &[String]) -> String {
    if tools.is_empty() {
        "none".to_string()
    } else {
        tools.join(", ")
    }
}
