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

use modron_mobile_core::view::NO_SESSION_LABEL;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use modron_mobile_core::{
    approval_mode_next, connection_label, sandbox_next, ClientError, ConfigPatch, ConnectionState,
    RemoteClient, ViewModelSnapshot, PLAN_DECISION_LABELS,
};
use modron_protocol::SessionMetaWire;
use tokio::sync::mpsc::{
    error::TryRecvError, unbounded_channel, UnboundedReceiver, UnboundedSender,
};
use uuid::Uuid;

use crate::chat::{Host, UiAction};

/// How often the driver is stepped.
pub const TICK: Duration = Duration::from_millis(20);

/// How long a session list may take to arrive. The rows land in the state's
/// `sessions`, and an empty list looks the same as a late one, so the wait
/// is bounded.
pub const LIST_WAIT: Duration = Duration::from_secs(2);

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
#[derive(Clone, Debug, PartialEq, Eq, Default)]
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    attempts: u32,
    last: String,
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
        match RemoteClient::connect(&options.url, options.token.as_deref()) {
            Ok(client) => Some(Self {
                client,
                options,
                status,
                actions,
                out,
                linked: false,
                stage: Stage::Ready,
                resume: None,
                retry_at: None,
                attempts: 0,
                last: String::new(),
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
        if state.session_id.is_some() {
            self.resume = state.session_id;
        }
        match state.connection {
            ConnectionState::Connected => {
                self.status.reconnecting.store(false, Ordering::SeqCst);
                if !self.linked {
                    self.linked = true;
                    self.attempts = 0;
                    self.retry_at = None;
                    self.link_up();
                }
            }
            ConnectionState::Connecting => {}
            ConnectionState::Disconnected => {
                self.linked = false;
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
                        self.status.fail(format!(
                            "no session to continue on {}: start one with modron-app-tui --new",
                            self.options.url
                        ));
                        return false;
                    }
                    None => {}
                }
            }
        }
        let mut snapshot = self.client.state();
        snapshot.connection_label = self.label(&snapshot.connection_label);
        // Snapshots carry no PartialEq; the JSON form is the change test.
        let json = serde_json::to_string(&snapshot).unwrap_or_default();
        if json != self.last {
            self.last = json;
            if self.out.send(snapshot).is_err() {
                return false;
            }
        }
        true
    }

    /// The connection label with the address (and the note, when set).
    fn label(&self, base: &str) -> String {
        let note = self.status.note();
        if note.is_empty() {
            format!("{base} \u{b7} {}", self.options.url)
        } else {
            format!("{base} \u{b7} {} \u{b7} {note}", self.options.url)
        }
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
        }
    }

    /// One reconnect try: re-attach to the known session, or connect afresh
    /// when no session was ever seen (the post-connect setup runs again).
    fn reconnect(&mut self) {
        let token = self.options.token.as_deref();
        match self.resume {
            Some(session) => {
                let _ = self.client.reconnect(&self.options.url, session, token);
            }
            None => {
                if let Ok(client) = RemoteClient::connect(&self.options.url, token) {
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

/// Maps one UI action to its core call. A call the core refuses (no session
/// attached, a closed link) is shown as the snapshot's error, so the user
/// sees why nothing happened. Server refusals arrive as `Payload::Error` and
/// land in the same field.
fn apply(client: &RemoteClient, action: UiAction) {
    let result = match action {
        UiAction::Send(text) => client.send_message(text),
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
        UiAction::Compact => client.compact(),
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
        UiAction::ListModels => client.router_defaults(),
        UiAction::CycleMode => {
            let mode = approval_mode_next(client.state().approval_mode);
            client.set_config(ConfigPatch { approval_mode: Some(mode), ..Default::default() })
        }
        UiAction::CycleSandbox => {
            let policy = sandbox_next(client.state().sandbox_policy);
            client.set_config(ConfigPatch { sandbox: Some(policy), ..Default::default() })
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
        UiAction::Notice(text) => {
            client.note_local_error(text);
            Ok(())
        }
        // Navigation is applied by the host before an action reaches here.
        UiAction::Nav(_) => Ok(()),
    };
    if let Err(error) = result {
        client.note_local_error(error.to_string());
    }
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
