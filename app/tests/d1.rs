//! Headless tests for parity packet D1: the tool detail panel (Ctrl+T), the
//! `!command` shell passthrough, `@file` completion, and the startup config
//! that the driver forwards to a scripted server after attach.

use std::{
    sync::{mpsc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use futures_util::{SinkExt, StreamExt};
use modron_app::{
    chat::{link, Host, UiAction},
    session::{self, LiveOptions},
    App, CLIENT,
};
use modron_mobile_core::{ConfigPatch, ConnectionState, Effort, ViewModelSnapshot};
use modron_protocol::{Frame, LiveState, Payload, PROTOCOL_VERSION};
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
    view: AppView,
    term: Terminal<TestBackend>,
    host: Host,
    wake: mpsc::Receiver<()>,
    // Last on purpose: fields drop in order, and the view (its engine
    // thread) must be gone before the next test may take the lock.
    _serial: MutexGuard<'static, ()>,
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

    /// Draws for a short while, so the engine applies what the host sent.
    /// Draws until the engine commits nothing for 50 ms, bounded at 5 s: a
    /// pushed snapshot is applied before the next key. The wake channel
    /// reports engine commits, so this waits on the engine, not on time.
    fn settle(&mut self) {
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            self.draw();
            if self.wake.recv_timeout(Duration::from_millis(50)).is_err() || Instant::now() >= end {
                return;
            }
        }
    }

    /// Waits until the screen shows `what`, waking on engine commits.
    fn settle_on(&mut self, what: &str) {
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            self.draw();
            if self.screen().contains(what) {
                return;
            }
            assert!(Instant::now() < end, "timed out waiting for {what}\n{}", self.screen());
            let _ = self.wake.recv_timeout(Duration::from_millis(20));
        }
    }

    /// Routes a key the way the real terminal host does.
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

fn ctrl(name: &str) -> Input {
    Input::Key {
        key: name.to_string(),
        modifiers: Modifiers { ctrl: true, alt: false, shift: false, cmd: false },
        release: false,
    }
}

fn snapshot_with_tools(rows: Vec<String>) -> ViewModelSnapshot {
    ViewModelSnapshot {
        connection: ConnectionState::Connected,
        connection_label: "connected".into(),
        tool_detail_title: "\u{23fa} 2 tools \u{b7} read \u{d7}1 \u{b7} bash \u{d7}1".into(),
        tool_detail: rows,
        ..Default::default()
    }
}

#[test]
fn ctrl_t_opens_the_tool_panel_with_the_snapshot_rows_and_esc_closes_it() {
    let mut h = Harness::new();
    h.host.push(&snapshot_with_tools(vec![
        "\u{2713} read \u{2014} 2.1 KB".into(),
        "\u{25cf} bash ls".into(),
    ]));
    h.settle();
    assert!(!h.screen().contains("bash ls"), "the panel starts closed");
    h.send(ctrl("t"));
    h.settle();
    let open = h.screen();
    assert!(open.contains("\u{2713} read \u{2014} 2.1 KB"), "{open}");
    assert!(open.contains("\u{25cf} bash ls"), "{open}");
    assert!(open.contains("\u{23fa} 2 tools"), "the title is the snapshot's: {open}");
    h.send(key("Escape"));
    h.settle();
    assert!(!h.screen().contains("bash ls"), "esc closes the panel");
}

#[test]
fn the_panel_pages_and_jumps_with_the_transcript_keys() {
    let mut h = Harness::new();
    let rows: Vec<String> = (0..30).map(|i| format!("row {i:02}")).collect();
    h.host.push(&snapshot_with_tools(rows));
    h.settle();
    h.send(ctrl("t"));
    h.settle();
    let open = h.screen();
    assert!(open.contains("row 29") && !open.contains("row 00"), "opens at the newest row");
    h.send(key("Home"));
    h.settle();
    let top = h.screen();
    assert!(top.contains("row 00") && !top.contains("row 29"), "home jumps to the top");
    h.send(key("PageDown"));
    h.settle();
    assert!(h.screen().contains("row 10"), "pgdn moves a page");
    h.send(ctrl("t"));
    h.settle();
    assert!(!h.screen().contains("row 10"), "ctrl+t closes it again");
}

#[test]
fn ctrl_t_with_no_tool_activity_sends_a_note_and_opens_nothing() {
    let mut h = Harness::new();
    h.host
        .push(&ViewModelSnapshot { connection: ConnectionState::Connected, ..Default::default() });
    h.settle();
    h.send(ctrl("t"));
    assert_eq!(h.action(), Some(UiAction::Notice("no tool activity yet".into())));
}

/// A connected, idle screen: the composer takes text once a snapshot shows.
fn connected(h: &mut Harness) {
    h.host.push(&ViewModelSnapshot {
        connection: ConnectionState::Connected,
        connection_label: "connected".into(),
        ..Default::default()
    });
    h.settle_on("connected");
}

#[test]
fn a_bang_line_is_a_shell_action_and_never_a_submit() {
    let mut h = Harness::new();
    connected(&mut h);
    h.type_text("!ls");
    h.settle();
    h.send(key("Enter"));
    assert_eq!(h.action(), Some(UiAction::Shell { command: "ls".into(), private: false }));
    h.settle();
    h.type_text("  !  pwd ");
    h.settle();
    h.send(key("Enter"));
    assert_eq!(h.action(), Some(UiAction::Shell { command: "pwd".into(), private: false }));
}

#[test]
fn at_completion_lists_workspace_files_and_tab_splices_the_path() {
    let mut h = Harness::new();
    connected(&mut h);
    h.type_text("@");
    // The first @ starts the workspace walk; give it time, then type the
    // fragment so the list is drawn from the finished index.
    std::thread::sleep(Duration::from_millis(600));
    h.type_text("Cargo.t");
    h.settle();
    assert!(h.screen().contains("Cargo"), "typed: {}", h.screen());
    let shown = h.screen();
    assert!(shown.contains("@Cargo.toml"), "{shown}");
    h.send(key("Tab"));
    h.settle();
    h.send(key("Enter"));
    assert_eq!(h.action(), Some(UiAction::Send("@Cargo.toml".into())));
}

/// A scripted server: welcome, then the attach, then a snapshot naming the
/// session; it reports every client frame after that, in order.
fn start_server(session: Uuid, frames: usize) -> (String, mpsc::Receiver<Payload>) {
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
            let mut ws: WebSocketStream<TcpStream> = accept_async(stream).await.expect("ws");
            let _hello = read_frame(&mut ws).await;
            send_frame(
                &mut ws,
                &Frame::event(
                    None,
                    0,
                    Payload::Welcome {
                        server_version: "0.1.3".into(),
                        protocol: PROTOCOL_VERSION,
                        commands: Vec::new(),
                    },
                ),
            )
            .await;
            let attach = read_frame(&mut ws).await;
            assert!(matches!(attach.payload, Payload::Attach { .. }), "{:?}", attach.payload);
            let live = LiveState::default();
            send_frame(
                &mut ws,
                &Frame::event(
                    Some(session),
                    0,
                    Payload::Snapshot { session_id: session, last_seq: 0, events: vec![], live },
                ),
            )
            .await;
            for _ in 0..frames {
                let frame = read_frame(&mut ws).await;
                report_tx.send(frame.payload).expect("report");
            }
            while ws.next().await.is_some() {}
        });
    });
    let url = url_rx.recv_timeout(TIMEOUT).expect("server url");
    (url, report_rx)
}

async fn read_frame(ws: &mut WebSocketStream<TcpStream>) -> Frame {
    loop {
        match ws.next().await.expect("server: socket closed").expect("server: read") {
            WsMessage::Text(json) => return serde_json::from_str(&json).expect("frame json"),
            WsMessage::Ping(_) | WsMessage::Pong(_) => continue,
            other => panic!("server: unexpected frame {other:?}"),
        }
    }
}

async fn send_frame(ws: &mut WebSocketStream<TcpStream>, frame: &Frame) {
    let json = serde_json::to_string(frame).expect("encode");
    ws.send(WsMessage::Text(json.into())).await.expect("send");
}

#[test]
fn startup_config_is_forwarded_in_order_right_after_attach() {
    let session = Uuid::from_u128(0xd1);
    let (url, reports) = start_server(session, 4);
    let patches = vec![
        ConfigPatch { effort: Some(Effort::High), ..ConfigPatch::default() },
        ConfigPatch { goal: Some("ship".into()), ..ConfigPatch::default() },
    ];
    let driver = session::spawn(LiveOptions {
        url,
        session: Some(session),
        startup: patches.clone(),
        ..LiveOptions::default()
    })
    .expect("driver");
    let mut got = Vec::new();
    let deadline = Instant::now() + TIMEOUT;
    while got.len() < 2 {
        assert!(Instant::now() < deadline, "server saw {got:?}");
        // The connect also sends auth_providers (the first-run check); only
        // the config frames are counted here.
        if let Ok(payload) = reports.recv_timeout(Duration::from_millis(20)) {
            if matches!(payload, Payload::SetConfig { .. }) {
                got.push(payload);
            }
        }
    }
    let sent: Vec<ConfigPatch> = got
        .into_iter()
        .map(|payload| match payload {
            Payload::SetConfig { patch } => patch,
            other => panic!("expected set_config, got {other:?}"),
        })
        .collect();
    assert_eq!(sent, patches);
    drop(driver);
}
