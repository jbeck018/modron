//! The live session driver, shared by both hosts. A [`Driver`] owns one
//! `RemoteClient`; each [`Driver::step`] applies the UI's pending actions,
//! attaches once the handshake lands, and publishes the snapshot when it
//! changed. The view side is [`Live`], which moves those snapshots into the
//! host and the host's actions back to the driver.
//!
//! Who calls `step`: the terminal host runs it on its own thread
//! ([`spawn`]); the web host runs it from the page's timer. The core needs no
//! runtime of its own on either target (native owns one; wasm uses
//! `spawn_local`), so the driver itself is plain sync code.
//!
//! The driver publishes a snapshot whenever the folded view state changes,
//! and stops after the connection goes `Disconnected` (reconnect is out of
//! scope): that final snapshot carries the core's `disconnected` label.

use std::time::Duration;

use modron_mobile_core::{
    connection_label, ConnectionState, RemoteClient, ViewModelSnapshot, PLAN_DECISION_LABELS,
};
use tokio::sync::mpsc::{
    error::TryRecvError, unbounded_channel, UnboundedReceiver, UnboundedSender,
};
use uuid::Uuid;

use crate::chat::{Host, UiAction};

/// How often the driver is stepped.
pub const TICK: Duration = Duration::from_millis(20);

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
        Ok(url.filter(|u| !u.is_empty()).map(|url| Self { url, token, session }))
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

/// The view's end of the driver.
pub struct Live {
    snapshots: UnboundedReceiver<ViewModelSnapshot>,
    actions: UnboundedSender<UiAction>,
}

impl Live {
    /// Creates the link: the view side, the driver's action receiver and the
    /// driver's snapshot sender.
    pub fn channels() -> (Self, UnboundedReceiver<UiAction>, UnboundedSender<ViewModelSnapshot>) {
        let (snap_tx, snap_rx) = unbounded_channel();
        let (act_tx, act_rx) = unbounded_channel();
        (Self { snapshots: snap_rx, actions: act_tx }, act_rx, snap_tx)
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
}

/// Starts the driver on its own thread (terminal host). Connecting is
/// asynchronous: the first snapshot shows `connecting`.
#[cfg(not(target_arch = "wasm32"))]
pub fn spawn(options: LiveOptions) -> std::io::Result<Live> {
    let (live, actions, out) = Live::channels();
    std::thread::Builder::new().name("modron-session".into()).spawn(move || {
        if let Some(mut driver) = Driver::start(options, actions, out) {
            while driver.step() {
                std::thread::sleep(TICK);
            }
        }
    })?;
    Ok(live)
}

/// The driver: one client, its attach state and the last snapshot sent.
pub struct Driver {
    client: RemoteClient,
    options: LiveOptions,
    actions: UnboundedReceiver<UiAction>,
    out: UnboundedSender<ViewModelSnapshot>,
    attached: bool,
    last: String,
}

impl Driver {
    /// Starts the client. `None` when the URL is refused: the failure is sent
    /// as a `Disconnected` snapshot first, so the view still shows it.
    pub fn start(
        options: LiveOptions,
        actions: UnboundedReceiver<UiAction>,
        out: UnboundedSender<ViewModelSnapshot>,
    ) -> Option<Self> {
        match RemoteClient::connect(&options.url, options.token.as_deref()) {
            Ok(client) => {
                Some(Self { client, options, actions, out, attached: false, last: String::new() })
            }
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

    /// One pass: apply the UI's actions, attach once connected, publish the
    /// snapshot if it changed. Returns false when the driver is finished (the
    /// view is gone, the link closed, or the connection is `Disconnected`).
    pub fn step(&mut self) -> bool {
        loop {
            match self.actions.try_recv() {
                Ok(action) => apply(&self.client, action),
                Err(TryRecvError::Empty) => break,
                // The view is gone: nothing left to drive.
                Err(TryRecvError::Disconnected) => return false,
            }
        }
        if !self.attached && self.client.is_connected() {
            self.attached = true;
            if let Some(session) = self.options.session {
                let _ = self.client.attach(session, 0);
            }
        }
        let snapshot = self.client.state();
        let closed = snapshot.connection == ConnectionState::Disconnected;
        // Snapshots carry no PartialEq; the JSON form is the change test.
        let json = serde_json::to_string(&snapshot).unwrap_or_default();
        if json != self.last {
            self.last = json;
            if self.out.send(snapshot).is_err() {
                return false;
            }
        }
        !closed
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        self.client.disconnect();
    }
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
            }))
        );
    }

    #[test]
    fn query_rejects_a_bad_session_and_keeps_bad_escapes() {
        assert!(LiveOptions::from_query("?url=ws://h&session=nope").is_err());
        assert_eq!(decode_form("100%"), "100%");
        assert_eq!(decode_form("%zz"), "%zz");
    }
}
