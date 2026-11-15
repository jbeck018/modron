//! The live session driver. One `RemoteClient` lives on a dedicated thread
//! that runs a single-threaded tokio runtime. The UI thread never touches
//! the client: it drains snapshots and hands over `UiAction`s, both through
//! unbounded channels ([`Live::pump`]).
//!
//! The driver publishes a snapshot whenever the folded view state changes,
//! and stops after the connection goes `Disconnected` (reconnect is out of
//! scope): that final snapshot carries the core's `disconnected` label.

use std::{io, thread, time::Duration};

use modron_mobile_core::{
    connection_label, ConnectionState, RemoteClient, ViewModelSnapshot, PLAN_DECISION_LABELS,
};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use uuid::Uuid;

use crate::chat::{Host, UiAction};

/// How often the driver checks the client for a changed snapshot.
const TICK: Duration = Duration::from_millis(20);

/// Where the live driver connects, and which session it attaches to. With
/// no session, the first submit starts one and the server names it.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct LiveOptions {
    pub url: String,
    pub token: Option<String>,
    pub session: Option<Uuid>,
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
            Some(url) => Ok(Some(Self { url, token, session })),
            None if token.is_some() || session.is_some() => {
                Err("--token and --session need --url".into())
            }
            None => Ok(None),
        }
    }
}

/// The UI thread's end of the driver.
pub struct Live {
    snapshots: UnboundedReceiver<ViewModelSnapshot>,
    actions: UnboundedSender<UiAction>,
}

/// Starts the driver thread. Connecting is asynchronous: the first snapshot
/// shows `connecting`.
pub fn spawn(options: LiveOptions) -> io::Result<Live> {
    let (snap_tx, snapshots) = unbounded_channel();
    let (actions, act_rx) = unbounded_channel();
    thread::Builder::new().name("modron-session".into()).spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("tokio runtime for the session driver");
        runtime.block_on(drive(options, act_rx, snap_tx));
    })?;
    Ok(Live { snapshots, actions })
}

impl Live {
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
}

/// The driver loop: connect, attach when the handshake lands, apply the
/// UI's actions, and publish every changed snapshot.
async fn drive(
    options: LiveOptions,
    mut actions: UnboundedReceiver<UiAction>,
    out: UnboundedSender<ViewModelSnapshot>,
) {
    let client = match RemoteClient::connect(&options.url, options.token.as_deref()) {
        Ok(client) => client,
        Err(e) => {
            let _ = out.send(ViewModelSnapshot {
                connection: ConnectionState::Disconnected,
                connection_label: connection_label(&ConnectionState::Disconnected).into(),
                last_error: Some(e.to_string()),
                ..Default::default()
            });
            return;
        }
    };
    let mut attached = false;
    let mut last = String::new();
    let mut tick = tokio::time::interval(TICK);
    loop {
        tokio::select! {
            action = actions.recv() => match action {
                Some(action) => apply(&client, action),
                // The view is gone: nothing left to drive.
                None => break,
            },
            _ = tick.tick() => {}
        }
        if !attached && client.is_connected() {
            attached = true;
            if let Some(session) = options.session {
                let _ = client.attach(session, 0);
            }
        }
        let snapshot = client.state();
        let closed = snapshot.connection == ConnectionState::Disconnected;
        // Snapshots carry no PartialEq; the JSON form is the change test.
        let json = serde_json::to_string(&snapshot).unwrap_or_default();
        if json != last {
            last = json;
            if out.send(snapshot).is_err() {
                break;
            }
        }
        if closed {
            break;
        }
    }
    client.disconnect();
}

/// Maps one UI action to its core call. A refused call (no session yet, or
/// a closed link) changes nothing here; the next snapshot shows the state.
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
    };
    let _ = result;
}
