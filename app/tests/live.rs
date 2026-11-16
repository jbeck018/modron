//! Headless tests for the live chat: queue chips, steer echoes and the plan
//! block rendered from fixture snapshots; key mapping for send, queue, plan
//! decisions and `/compact`; and one end-to-end run where the session driver
//! talks to a scripted in-process WebSocket server.

use std::{
    sync::{mpsc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use futures_util::{SinkExt, StreamExt};
use modron_app::{
    chat::{link, Host, UiAction},
    session::{self, Live, LiveOptions},
    App, CLIENT,
};
use modron_mobile_core::{
    entry_lines, ConnectionState, DecisionOption, PendingApprovalView, PendingPlanView,
    PlanDecisionOption, RenderedRow, ViewModelSnapshot, DECISION_LABELS, PLAN_DECISION_LABELS,
    UNDECODABLE_NOTE,
};
use modron_protocol::{
    AuthProviderWire, Frame, LiveState, Payload, PendingApproval, PersonaWire, SessionMetaWire,
    WireDelta, PROTOCOL_VERSION,
};
use modron_types::{ToolCall, Usage};
use modron_view::{GenUi, TranscriptEntry};
use nsh_platform_tui::{AppOptions, AppView, CacheOptions, Input, Modifiers, Root};
use ratatui::{backend::TestBackend, Terminal};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{accept_async, WebSocketStream};
use uuid::Uuid;

const COLS: u16 = 120;
const ROWS: u16 = 40;
const TIMEOUT: Duration = Duration::from_secs(10);

/// The view reads a process-wide slot, so the tests run one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

struct Harness {
    _serial: MutexGuard<'static, ()>,
    view: AppView,
    term: Terminal<TestBackend>,
    host: Host,
    wake: mpsc::Receiver<()>,
}

impl Harness {
    fn new() -> Self {
        let serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (host, client) = link();
        *CLIENT.lock().expect("client slot") = Some(client);
        let (tx, wake) = mpsc::channel();
        let mut options = AppOptions::default();
        options.wake = Some(std::sync::Arc::new(move || {
            let _ = tx.send(());
        }));
        options.cache = CacheOptions::off();
        let mut view = AppView::new(options);
        view.load(Root::of::<App>()).expect("load root");
        view.set_focused(true);
        let term = Terminal::new(TestBackend::new(COLS, ROWS)).expect("terminal");
        Self { _serial: serial, view, term, host, wake }
    }

    fn draw(&mut self) {
        self.term.draw(|f| self.view.render(f, f.area())).expect("draw");
    }

    fn screen(&self) -> String {
        let buf = self.term.backend().buffer();
        buf.content
            .chunks(COLS as usize)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Draws until the screen satisfies `done`, waking on engine commits.
    /// With a live driver, its snapshots and actions are pumped each round.
    fn wait_for(&mut self, what: &str, live: Option<&mut Live>, done: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        let mut live = live;
        loop {
            if let Some(driver) = live.as_mut() {
                driver.pump(&mut self.host);
            }
            self.draw();
            if done(&self.screen()) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}\n{}", self.screen());
            let due = self.view.next_frame_deadline().map_or(deadline, |d| d.min(deadline));
            let wait = due.saturating_duration_since(Instant::now()).min(Duration::from_millis(10));
            let _ = self.wake.recv_timeout(wait);
        }
    }

    /// Routes a key the way the real host does: the link's filter first,
    /// then the NSH view.
    /// Returns true when the view got the key (the host did not consume it).
    fn send(&mut self, input: Input) -> bool {
        if let Some(input) = self.host.filter(input) {
            self.view.handle(input);
            true
        } else {
            false
        }
    }

    /// Waits until the engine has applied `want` as the composer text. The
    /// host routes Enter from that text, so the next key must not race it.
    fn wait_draft(&mut self, want: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            self.draw();
            if modron_app::chat::composer_text() == want {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for composer text {want:?}\n{}",
                self.screen()
            );
            let _ = self.wake.recv_timeout(std::time::Duration::from_millis(20));
        }
    }

    /// Types one key at a time; a key the composer gets waits until the
    /// engine has applied it (Enter after the text reads the whole text).
    fn type_text(&mut self, text: &str) {
        for c in text.chars() {
            let want = format!("{}{c}", modron_app::chat::composer_text());
            if self.send(key(&c.to_string())) {
                self.wait_draft(&want);
            }
        }
    }

    /// Pushes a snapshot as if its prompt had shown a second ago, so the
    /// approval type-ahead window (300 ms) is closed for the keys sent at
    /// once. The window has its own tests in tests/chat.rs.
    fn push(&mut self, snapshot: &ViewModelSnapshot) {
        self.host.push_at(snapshot, modron_app::clock::Instant::now() - Duration::from_secs(1));
    }

    /// The next action the view sent, waiting briefly for the engine.
    fn action(&mut self) -> Option<UiAction> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(action) = self.host.try_action() {
                return Some(action);
            }
            if Instant::now() >= deadline {
                return None;
            }
            self.draw();
            let _ = self.wake.recv_timeout(Duration::from_millis(20));
        }
    }
}

fn key(name: &str) -> Input {
    Input::Key { key: name.to_string(), modifiers: Modifiers::default(), release: false }
}

fn alt_key(name: &str) -> Input {
    Input::Key {
        key: name.to_string(),
        modifiers: Modifiers { alt: true, ..Modifiers::default() },
        release: false,
    }
}

/// The composer shows its placeholder only when the draft is empty.
fn composer_is_empty(screen: &str) -> bool {
    screen.lines().any(|line| line.trim() == "Message")
}

fn row(entry: TranscriptEntry) -> RenderedRow {
    RenderedRow { lines: entry_lines(&entry), entry }
}

fn fixture() -> ViewModelSnapshot {
    ViewModelSnapshot {
        connection: ConnectionState::Connected,
        connection_label: "connected".into(),
        running: true,
        running_label: "running".into(),
        status_line:
            "claude-opus-5 \u{b7} 18.4k tok \u{b7} cache 71% \u{b7} plan \u{b7} ctx 41% (74k/180k)"
                .into(),
        model: "claude-opus-5".into(),
        provider: "anthropic".into(),
        history: vec![
            row(TranscriptEntry::Task { text: "fix the router crate".into() }),
            row(TranscriptEntry::Assistant { text: "Looking at the router now.".into() }),
        ],
        ..Default::default()
    }
}

fn plan(snapshot: &mut ViewModelSnapshot) {
    snapshot.plan_pending = Some(PendingPlanView {
        lines: vec!["[ ] read the router".into(), "[ ] add the test".into()],
        options: PLAN_DECISION_LABELS
            .map(|(decision, label)| PlanDecisionOption { decision, label: label.into() }),
    });
}

#[test]
fn queue_chips_render_above_the_composer() {
    let mut h = Harness::new();
    let mut snapshot = fixture();
    snapshot.queued = vec!["fix the tests".into(), "then update the docs".into()];
    h.push(&snapshot);
    h.wait_for("queue chips", None, |s| s.contains("[2] then update the docs"));
    let screen = h.screen();
    assert!(screen.contains("[1] fix the tests"), "{screen}");
    assert!(screen.find("[2] then update the docs") > screen.find("[1] fix the tests"));
}

#[test]
fn steer_echoes_render_after_the_transcript() {
    let mut h = Harness::new();
    let mut snapshot = fixture();
    snapshot.pending_steers = vec!["also check the parser".into()];
    h.push(&snapshot);
    h.wait_for("steer echo", None, |s| s.contains("> also check the parser"));
    let screen = h.screen();
    let reply = screen.find("Looking at the router now.").expect("transcript row");
    let steer = screen.find("> also check the parser").expect("steer echo");
    assert!(steer > reply, "steer echo must follow the transcript\n{screen}");
}

#[test]
fn plan_block_renders_lines_and_core_option_labels() {
    let mut h = Harness::new();
    let mut snapshot = fixture();
    plan(&mut snapshot);
    h.push(&snapshot);
    h.wait_for("plan block", None, |s| s.contains("y approve"));
    let screen = h.screen();
    for text in ["[ ] read the router", "[ ] add the test", "y approve", "r revise"] {
        assert!(screen.contains(text), "missing {text:?}\n{screen}");
    }
}

#[test]
fn enter_sends_and_alt_enter_queues_the_composer_text() {
    let mut h = Harness::new();
    h.push(&fixture());
    h.wait_for("transcript", None, |s| s.contains("fix the router crate"));
    h.type_text("first");
    h.wait_for("typed first", None, |s| s.contains("first"));
    h.send(key("Enter"));
    assert_eq!(h.action(), Some(UiAction::Send("first".into())));
    h.wait_for("composer cleared", None, composer_is_empty);
    h.type_text("later");
    h.wait_for("typed later", None, |s| s.contains("later"));
    h.send(alt_key("Enter"));
    assert_eq!(h.action(), Some(UiAction::Queue("later".into())));
    h.wait_for("composer cleared after queue", None, composer_is_empty);
    // The flag resets: a plain Enter after a queue sends again.
    h.type_text("again");
    h.wait_for("typed again", None, |s| s.contains("again"));
    h.send(key("Enter"));
    assert_eq!(h.action(), Some(UiAction::Send("again".into())));
}

#[test]
fn slash_compact_sends_compact_instead_of_text() {
    let mut h = Harness::new();
    h.push(&fixture());
    h.wait_for("transcript", None, |s| s.contains("fix the router crate"));
    h.type_text("/compact");
    // The suggestion row shows "/compact" while the composer may still hold
    // a prefix, so wait until the engine has applied the whole word.
    h.wait_draft("/compact");
    h.send(key("Enter"));
    assert_eq!(h.action(), Some(UiAction::Compact));
}

#[test]
fn y_approves_and_r_revises_a_pending_plan() {
    let mut h = Harness::new();
    let mut snapshot = fixture();
    plan(&mut snapshot);
    h.push(&snapshot);
    h.wait_for("plan", None, |s| s.contains("r revise"));
    h.send(key("y"));
    assert_eq!(h.action(), Some(UiAction::DecidePlan { approve: true, note: None }));
    // Revise with an empty composer: no note.
    h.send(key("r"));
    assert_eq!(h.action(), Some(UiAction::DecidePlan { approve: false, note: None }));
}

#[test]
fn r_revises_with_the_composer_text_as_the_note() {
    let mut h = Harness::new();
    h.push(&fixture());
    h.wait_for("transcript", None, |s| s.contains("fix the router crate"));
    // Typed before the plan arrives: the plan keys are taken by then.
    h.type_text("fix it");
    h.wait_for("typed note", None, |s| s.contains("fix it"));
    let mut snapshot = fixture();
    plan(&mut snapshot);
    h.push(&snapshot);
    h.wait_for("plan", None, |s| s.contains("r revise"));
    h.send(key("r"));
    assert_eq!(
        h.action(),
        Some(UiAction::DecidePlan { approve: false, note: Some("fix it".into()) })
    );
}

#[test]
fn plan_keys_are_plain_text_when_no_plan_waits() {
    let mut h = Harness::new();
    h.push(&fixture());
    h.wait_for("transcript", None, |s| s.contains("fix the router crate"));
    h.type_text("yr");
    h.wait_for("typed yr", None, |s| s.contains("yr"));
    assert_eq!(h.action(), None);
}

#[test]
fn backspace_on_an_empty_composer_dequeues_the_last_message() {
    let mut h = Harness::new();
    let mut snapshot = fixture();
    snapshot.queued = vec!["fix the tests".into(), "then update the docs".into()];
    h.push(&snapshot);
    h.wait_for("queue chips", None, |s| s.contains("[2] then update the docs"));
    h.send(key("Backspace"));
    assert_eq!(h.action(), Some(UiAction::Dequeue(1)));
}

#[test]
fn backspace_does_nothing_without_a_queue_or_with_composer_text() {
    let mut h = Harness::new();
    h.push(&fixture());
    h.wait_for("transcript", None, |s| s.contains("fix the router crate"));
    h.send(key("Backspace"));
    assert_eq!(h.action(), None, "empty queue");
    let mut queued = fixture();
    queued.queued = vec!["later".into()];
    h.push(&queued);
    h.wait_for("queue chip", None, |s| s.contains("[1] later"));
    h.type_text("zed");
    h.wait_for("typed zed", None, |s| s.contains("zed"));
    h.send(key("Backspace"));
    assert_eq!(h.action(), None, "composer has text: Backspace edits it");
}

// ---------------------------------------------------------------------------
// End to end: the session driver against a scripted server.
// ---------------------------------------------------------------------------

type Ws = WebSocketStream<TcpStream>;

async fn read_frame(ws: &mut Ws) -> Frame {
    loop {
        match ws.next().await.expect("server: socket closed").expect("server: read") {
            WsMessage::Text(json) => {
                return serde_json::from_str(&json).expect("server: frame json")
            }
            WsMessage::Ping(_) | WsMessage::Pong(_) => continue,
            other => panic!("server: unexpected frame {other:?}"),
        }
    }
}

async fn send_frame(ws: &mut Ws, frame: &Frame) {
    let json = serde_json::to_string(frame).expect("server: encode");
    ws.send(WsMessage::Text(json.into())).await.expect("server: send");
}

/// How the scripted server behaves after the welcome.
#[derive(Clone, Default)]
struct Script {
    /// Share link: no attach is sent; the server names the session in a
    /// snapshot right after the welcome.
    share_link: bool,
    /// The attached session is running (the composer then steers).
    running: bool,
    /// Answer the first client request after the stream with this error.
    refusal: Option<&'static str>,
    /// The first snapshot carries a pending `bash` approval ("req-1").
    approval: bool,
    /// One text frame the client cannot decode, sent before the snapshot.
    garbage: bool,
    /// The Welcome names a protocol one version ahead of the client's.
    protocol_skew: bool,
    /// The personas `personas_req` is answered with. None: the request is
    /// reported and not answered.
    personas: Option<Vec<PersonaWire>>,
    /// The rows `auth_providers_req` is answered with.
    providers: Vec<AuthProviderWire>,
}

/// Starts the scripted server on its own thread. Returns its ws URL and a
/// channel that yields the payload of the first client frame after the
/// stream (the submit or steer under test).
///
/// Script: hello -> welcome; then either attach -> snapshot, or (share
/// link) snapshot at once; then a text delta; then the first request is
/// reported back, and refused when the script says so.
fn start_server(session: Uuid, script: Script) -> (String, mpsc::Receiver<Payload>) {
    let (url_tx, url_rx) = mpsc::channel();
    let (report_tx, report_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("server runtime");
        runtime.block_on(async move {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("addr");
            url_tx.send(format!("ws://{addr}")).expect("send url");
            let (stream, _) = listener.accept().await.expect("accept");
            let mut ws = accept_async(stream).await.expect("ws accept");

            let hello = read_frame(&mut ws).await;
            assert!(matches!(hello.payload, Payload::Hello { .. }), "expected hello");
            send_frame(
                &mut ws,
                &Frame::event(
                    None,
                    0,
                    Payload::Welcome {
                        server_version: "0.1.3".into(),
                        protocol: PROTOCOL_VERSION + u32::from(script.protocol_skew),
                    },
                ),
            )
            .await;
            if script.garbage {
                ws.send(WsMessage::Text("not a frame".into())).await.expect("server: garbage");
            }

            if !script.share_link {
                let attach = read_frame(&mut ws).await;
                assert!(
                    matches!(attach.payload, Payload::Attach { session_id, .. } if session_id == session),
                    "expected attach to the session, got {:?}",
                    attach.payload
                );
            }
            let pending_approval = script.approval.then(|| PendingApproval {
                request_id: "req-1".into(),
                call: ToolCall {
                    id: "call-1".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({ "command": "ls" }),
                },
                agent: None,
            });
            let live =
                LiveState { running: script.running, pending_approval, ..LiveState::default() };
            send_frame(
                &mut ws,
                &Frame::event(
                    Some(session),
                    0,
                    Payload::Snapshot { session_id: session, last_seq: 0, events: vec![], live },
                ),
            )
            .await;
            send_frame(
                &mut ws,
                &Frame::event(
                    Some(session),
                    1,
                    Payload::Delta {
                        delta: WireDelta::Text { text: "Migrating the router now".into() },
                    },
                ),
            )
            .await;

            // The connect also sends auth_providers and list_sessions: the
            // scripted rows answer auth_providers, and the scripted personas
            // answer personas_req (both reported, where a request is). Any
            // other request ends the loop and is the one the test checks.
            let request = loop {
                let frame = read_frame(&mut ws).await;
                match &frame.payload {
                    Payload::AuthProvidersReq { .. } => {
                        let rows = script.providers.clone();
                        send_frame(
                            &mut ws,
                            &Frame::event(Some(session), 3, Payload::AuthProviders { rows }),
                        )
                        .await;
                    }
                    Payload::ListSessions {} => {}
                    Payload::PersonasReq {} if script.personas.is_some() => {
                        let personas = script.personas.clone().unwrap_or_default();
                        send_frame(
                            &mut ws,
                            &Frame::event(Some(session), 4, Payload::Personas { personas }),
                        )
                        .await;
                        report_tx.send(Payload::PersonasReq {}).expect("report request");
                    }
                    _ => break frame,
                }
            };
            report_tx.send(request.payload).expect("report request");
            if let Some(message) = script.refusal {
                send_frame(
                    &mut ws,
                    &Frame::event(
                        Some(session),
                        2,
                        Payload::Error { message: message.into(), kind: None },
                    ),
                )
                .await;
                // A live server keeps the socket open: hold it until the
                // client closes, so the disconnect does not replace the error.
                while ws.next().await.is_some() {}
            }
        });
    });
    let url = url_rx.recv_timeout(TIMEOUT).expect("server url");
    (url, report_rx)
}

/// Pumps the driver until the approval type-ahead window has closed (the
/// app clock; see `chat::prompt_guard_open`).
fn wait_past_guard(h: &mut Harness, live: &mut Live) {
    let deadline = Instant::now() + TIMEOUT;
    while modron_app::chat::prompt_guard_open(modron_app::clock::Instant::now()) {
        live.pump(&mut h.host);
        h.draw();
        assert!(Instant::now() < deadline, "the type-ahead window never closed\n{}", h.screen());
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Pumps the driver until the server reports the first request.
fn next_request(h: &mut Harness, live: &mut Live, reports: &mpsc::Receiver<Payload>) -> Payload {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        live.pump(&mut h.host);
        h.draw();
        if let Ok(payload) = reports.try_recv() {
            return payload;
        }
        assert!(Instant::now() < deadline, "server got no request\n{}", h.screen());
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn driver_renders_the_stream_and_steers_a_typed_message() {
    let session = Uuid::new_v4();
    let (url, reports) = start_server(session, Script::default());
    let mut live = session::spawn(LiveOptions {
        url,
        token: None,
        session: Some(session),
        ..Default::default()
    })
    .expect("spawn driver");
    let mut h = Harness::new();

    h.wait_for("streamed text", Some(&mut live), |s| s.contains("Migrating the router now"));
    assert!(h.screen().contains("connected"), "{}", h.screen());

    h.type_text("hi");
    h.wait_for("typed hi", Some(&mut live), |s| s.contains("hi"));
    h.send(key("Enter"));
    match next_request(&mut h, &mut live, &reports) {
        // A delta is streaming, so the session is running: the composer
        // steers (PROTOCOL_V2 section 2) instead of submitting.
        Payload::Steer { session_id, text } => {
            assert_eq!(session_id, session);
            assert_eq!(text, "hi");
        }
        other => panic!("expected steer, got {other:?}"),
    }
}

#[test]
fn a_share_link_adopts_the_session_its_snapshot_names() {
    // No session in the options and no attach: the snapshot names it, and
    // the typed message goes to that session.
    let session = Uuid::new_v4();
    let script = Script { share_link: true, ..Script::default() };
    let (url, reports) = start_server(session, script);
    let mut live =
        session::spawn(LiveOptions { url, token: None, session: None, ..Default::default() })
            .expect("spawn driver");
    let mut h = Harness::new();

    h.wait_for("streamed text", Some(&mut live), |s| s.contains("Migrating the router now"));
    h.type_text("hi");
    h.wait_for("typed hi", Some(&mut live), |s| s.contains("hi"));
    h.send(key("Enter"));
    match next_request(&mut h, &mut live, &reports) {
        // The stream implies a running session, so this is a steer for
        // the adopted session.
        Payload::Steer { session_id, text } => {
            assert_eq!(session_id, session);
            assert_eq!(text, "hi");
        }
        other => panic!("expected steer for the adopted session, got {other:?}"),
    }
}

#[test]
fn a_share_link_steers_when_the_adopted_session_is_running() {
    let session = Uuid::new_v4();
    let script = Script { share_link: true, running: true, ..Script::default() };
    let (url, reports) = start_server(session, script);
    let mut live =
        session::spawn(LiveOptions { url, token: None, session: None, ..Default::default() })
            .expect("spawn driver");
    let mut h = Harness::new();

    h.wait_for("streamed text", Some(&mut live), |s| s.contains("Migrating the router now"));
    h.type_text("hi");
    h.wait_for("typed hi", Some(&mut live), |s| s.contains("hi"));
    h.send(key("Enter"));
    match next_request(&mut h, &mut live, &reports) {
        Payload::Steer { session_id, text } => {
            assert_eq!(session_id, session);
            assert_eq!(text, "hi");
        }
        other => panic!("expected steer for the adopted session, got {other:?}"),
    }
}

#[test]
fn a_refused_send_shows_the_server_error_on_screen() {
    let session = Uuid::new_v4();
    let refusal = "single-session tunnel: submit refused";
    let script = Script { refusal: Some(refusal), ..Script::default() };
    let (url, reports) = start_server(session, script);
    let mut live = session::spawn(LiveOptions {
        url,
        token: None,
        session: Some(session),
        ..Default::default()
    })
    .expect("spawn driver");
    let mut h = Harness::new();

    h.wait_for("streamed text", Some(&mut live), |s| s.contains("Migrating the router now"));
    h.type_text("hi");
    h.wait_for("typed hi", Some(&mut live), |s| s.contains("hi"));
    h.send(key("Enter"));
    let _ = next_request(&mut h, &mut live, &reports);
    h.wait_for("refusal on screen", Some(&mut live), |s| s.contains(refusal));
}

// ---------------------------------------------------------------------------
// Rendering of core entries and notes, and the wire calls behind the
// slash commands, approvals and handshake (one scripted server per test).
// ---------------------------------------------------------------------------

fn pending_view() -> PendingApprovalView {
    PendingApprovalView {
        request_id: "req-1".into(),
        tool: "edit".into(),
        args_summary: "crates/modron-router/src/lib.rs".into(),
        arguments: serde_json::json!({}),
        arguments_text: "{}".into(),
        options: DECISION_LABELS
            .iter()
            .map(|(decision, label)| DecisionOption { decision: *decision, label: (*label).into() })
            .collect(),
    }
}

#[test]
fn an_interrupted_turn_note_stays_in_the_notes_row() {
    let mut h = Harness::new();
    let mut snapshot = fixture();
    snapshot.last_note = Some("turn interrupted by the user".into());
    h.push(&snapshot);
    h.wait_for("interrupted note", None, |s| s.contains("turn interrupted by the user"));
}

#[test]
fn turn_end_usage_swarm_and_generative_rows_render_from_the_core_entries() {
    let mut h = Harness::new();
    let mut snapshot = fixture();
    snapshot.history.push(row(TranscriptEntry::Usage {
        model: "claude-opus-5".into(),
        usage: Usage { input_tokens: 1200, output_tokens: 300, cached_input_tokens: 600 },
    }));
    snapshot.history.push(row(TranscriptEntry::Swarm {
        lines: vec!["[scout] done - 3 turns - last: read".into()],
    }));
    snapshot.history.push(row(TranscriptEntry::GenerativeUi {
        ui: GenUi::Table {
            title: None,
            columns: vec!["file".into()],
            rows: vec![vec!["router-table-cell.rs".into()]],
        },
    }));
    h.push(&snapshot);
    h.wait_for("usage row", None, |s| s.contains("1500 tok"));
    let screen = h.screen();
    assert!(screen.contains("cache 50%"), "{screen}");
    assert!(screen.contains("[scout] done - 3 turns"), "{screen}");
    assert!(screen.contains("router-table-cell.rs"), "{screen}");
}

#[test]
fn an_approval_that_leaves_the_snapshot_clears_the_prompt() {
    let mut h = Harness::new();
    let mut snapshot = fixture();
    snapshot.pending_approval = Some(pending_view());
    h.push(&snapshot);
    h.wait_for("approval labels", None, |s| s.contains("allow once"));
    // Resolved elsewhere: the next snapshot has no pending approval.
    h.push(&fixture());
    h.wait_for("prompt cleared", None, |s| !s.contains("allow once"));
}

/// Types `command`, submits it, and returns the first request the scripted
/// server reads after the stream.
fn wire_request(script: Script, command: &str) -> Payload {
    let session = Uuid::new_v4();
    let (url, reports) = start_server(session, script);
    let mut live = session::spawn(LiveOptions {
        url,
        token: None,
        session: Some(session),
        ..Default::default()
    })
    .expect("spawn driver");
    let mut h = Harness::new();
    h.wait_for("streamed text", Some(&mut live), |s| s.contains("Migrating the router now"));
    h.type_text(command);
    h.wait_draft(command);
    h.send(key("Enter"));
    next_request(&mut h, &mut live, &reports)
}

#[test]
fn fork_sends_a_fork_frame() {
    match wire_request(Script::default(), "/fork") {
        Payload::Fork { .. } => {}
        other => panic!("expected fork, got {other:?}"),
    }
}

#[test]
fn share_publishes_the_transcript_page() {
    match wire_request(Script::default(), "/share") {
        Payload::PublishPage { .. } => {}
        other => panic!("expected publish_page, got {other:?}"),
    }
}

#[test]
fn remote_control_sends_the_bridge_url_it_was_given() {
    match wire_request(Script::default(), "/remote-control wss://relay.test/rc") {
        Payload::RemoteControl { bridge_url, .. } => assert_eq!(bridge_url, "wss://relay.test/rc"),
        other => panic!("expected remote_control, got {other:?}"),
    }
}

#[test]
fn agent_sends_the_persona_as_a_config_patch() {
    let request = wire_request(Script::default(), "/agent reviewer");
    assert!(format!("{request:?}").contains("persona: Some(\"reviewer\")"), "{request:?}");
}

#[test]
fn allow_and_deny_send_the_tool_lists_as_config_patches() {
    let allow = wire_request(Script::default(), "/allow read");
    assert!(format!("{allow:?}").contains("allow: Some([\"read\"])"), "{allow:?}");
    let deny = wire_request(Script::default(), "/deny bash");
    assert!(format!("{deny:?}").contains("deny: Some([\"bash\"])"), "{deny:?}");
}

#[test]
fn sandbox_sends_the_policy_as_a_config_patch() {
    let request = wire_request(Script::default(), "/sandbox off");
    assert!(format!("{request:?}").contains("sandbox: Some("), "{request:?}");
}

#[test]
fn y_sends_the_approval_decision_over_the_wire() {
    let session = Uuid::new_v4();
    let script = Script { approval: true, ..Script::default() };
    let (url, reports) = start_server(session, script);
    let mut live = session::spawn(LiveOptions {
        url,
        token: None,
        session: Some(session),
        ..Default::default()
    })
    .expect("spawn driver");
    let mut h = Harness::new();
    h.wait_for("approval labels", Some(&mut live), |s| s.contains("allow once"));
    // A decision key inside the type-ahead window is ignored: wait it out
    // (on the app clock) as a user reading the prompt would.
    wait_past_guard(&mut h, &mut live);
    h.send(key("y"));
    match next_request(&mut h, &mut live, &reports) {
        Payload::Approve { request_id, .. } => assert_eq!(request_id, "req-1"),
        other => panic!("expected approve, got {other:?}"),
    }
}

#[test]
fn an_undecodable_frame_shows_the_shared_notice() {
    let session = Uuid::new_v4();
    let script = Script { garbage: true, ..Script::default() };
    let (url, _reports) = start_server(session, script);
    let mut live = session::spawn(LiveOptions {
        url,
        token: None,
        session: Some(session),
        ..Default::default()
    })
    .expect("spawn driver");
    let mut h = Harness::new();
    h.wait_for("undecodable notice", Some(&mut live), |s| s.contains(UNDECODABLE_NOTE));
}

#[test]
fn a_server_one_protocol_ahead_is_refused_and_never_streams() {
    let session = Uuid::new_v4();
    let script = Script { protocol_skew: true, ..Script::default() };
    let (url, _reports) = start_server(session, script);
    let mut live = session::spawn(LiveOptions {
        url,
        token: None,
        session: Some(session),
        ..Default::default()
    })
    .expect("spawn driver");
    let mut h = Harness::new();
    // A Welcome with another protocol never sets the connection up: the
    // header reads disconnected and the server's stream is not folded.
    h.wait_for("refused handshake", Some(&mut live), |s| s.contains("disconnected"));
    assert!(!h.screen().contains("Migrating the router now"), "{}", h.screen());
}

#[test]
fn reasoning_on_asks_the_provider_for_a_summary() {
    // The first toggle turns the display on, so the summary is asked for
    // (the scripted server reports only the first request after the stream).
    match wire_request(Script::default(), "/reasoning") {
        Payload::SetConfig { patch } => assert_eq!(patch.reasoning_summary, Some(true)),
        other => panic!("expected set_config, got {other:?}"),
    }
}

#[test]
fn agent_picker_asks_for_personas_and_enter_sends_the_chosen_one() {
    let session = Uuid::new_v4();
    let script = Script {
        personas: Some(vec![
            PersonaWire { name: "planner".into(), model: None, summary: "plans the work".into() },
            PersonaWire { name: "reviewer".into(), model: None, summary: String::new() },
        ]),
        ..Script::default()
    };
    let (url, reports) = start_server(session, script);
    let mut live = session::spawn(LiveOptions {
        url,
        token: None,
        session: Some(session),
        ..Default::default()
    })
    .expect("spawn driver");
    let mut h = Harness::new();
    h.wait_for("streamed text", Some(&mut live), |s| s.contains("Migrating the router now"));
    h.type_text("/agent");
    h.wait_draft("/agent");
    h.send(key("Enter"));
    assert!(matches!(next_request(&mut h, &mut live, &reports), Payload::PersonasReq {}));
    h.wait_for("the persona rows", Some(&mut live), |s| s.contains("planner - plans the work"));
    h.send(key("ArrowDown"));
    h.wait_for("the reviewer row selected", Some(&mut live), |s| s.contains("> reviewer"));
    h.send(key("Enter"));
    match next_request(&mut h, &mut live, &reports) {
        Payload::SetConfig { patch } => assert_eq!(patch.persona.as_deref(), Some("reviewer")),
        other => panic!("expected set_config, got {other:?}"),
    }
}

#[test]
fn d_on_a_signed_in_provider_asks_then_y_sends_auth_remove() {
    let session = Uuid::new_v4();
    let script = Script {
        providers: vec![AuthProviderWire {
            id: "anthropic".into(),
            label: "Anthropic".into(),
            status: "configured".into(),
            oauth_capable: true,
        }],
        ..Script::default()
    };
    let (url, reports) = start_server(session, script);
    let mut live = session::spawn(LiveOptions {
        url,
        token: None,
        session: Some(session),
        ..Default::default()
    })
    .expect("spawn driver");
    let mut h = Harness::new();
    h.wait_for("streamed text", Some(&mut live), |s| s.contains("Migrating the router now"));
    h.type_text("/auth");
    h.wait_draft("/auth");
    h.send(key("Enter"));
    h.wait_for("the signed-in row", Some(&mut live), |s| s.contains("Anthropic: configured"));
    h.send(key("d"));
    h.wait_for("the removal question", Some(&mut live), |s| {
        s.contains("remove Anthropic credentials? y / n")
    });
    h.send(key("y"));
    match next_request(&mut h, &mut live, &reports) {
        Payload::AuthRemove { provider, .. } => assert_eq!(provider, "anthropic"),
        other => panic!("expected auth_remove, got {other:?}"),
    }
}

#[test]
fn the_tasks_screen_renders_the_core_summary_under_the_row() {
    let mut snapshot = ViewModelSnapshot {
        connection: ConnectionState::Connected,
        connection_label: "connected".into(),
        ..Default::default()
    };
    snapshot.sessions = vec![SessionMetaWire {
        id: Uuid::from_u128(1),
        title: "t".into(),
        created_at: String::new(),
        updated_at: String::new(),
        message_count: 0,
        working_dir: None,
        task_branch: Some("task/fix".into()),
        task_state: Some("idle".into()),
    }];
    snapshot.task_rows = vec![modron_mobile_core::view::TaskRowView {
        session_id: Uuid::from_u128(1),
        text: "fix parser - task/fix - idle".into(),
        summary: "ahead 3 - 1 file (+120 -14)".into(),
    }];
    let mut h = Harness::new();
    h.push(&snapshot);
    h.wait_for("composer", None, |s| s.contains("type a task"));
    h.type_text("/tasks");
    h.wait_draft("/tasks");
    h.send(key("Enter"));
    assert_eq!(h.action(), Some(UiAction::ListSessions));
    h.wait_for("the summary", None, |s| s.contains("ahead 3 - 1 file (+120 -14)"));
    assert!(h.screen().contains("fix parser - task/fix - idle"));
}

#[test]
fn the_agent_picker_renders_the_personas_the_snapshot_names() {
    let mut snapshot = ViewModelSnapshot {
        connection: ConnectionState::Connected,
        connection_label: "connected".into(),
        ..Default::default()
    };
    snapshot.personas = vec![modron_mobile_core::view::PersonaRowView {
        name: "reviewer".into(),
        text: "reviewer - reviews diffs".into(),
        current: true,
    }];
    let mut h = Harness::new();
    h.push(&snapshot);
    h.wait_for("composer", None, |s| s.contains("type a task"));
    h.type_text("/agent");
    h.wait_draft("/agent");
    h.send(key("Enter"));
    assert_eq!(h.action(), Some(UiAction::ListPersonas));
    h.wait_for("the persona row", None, |s| s.contains("reviewer - reviews diffs"));
}
