//! Headless tests for navigation: the Sessions and Settings screens in NSH's
//! AppView rendered into ratatui's TestBackend (120x40), with the same
//! harness as tests/chat.rs. Keys go through the host's filter first, as the
//! terminal host sends them.

use std::{
    sync::{mpsc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use modron_app::{
    chat::{link, Host, UiAction},
    App, CLIENT,
};
use modron_mobile_core::{session_row, ApprovalModeWire, SessionMetaWire, ViewModelSnapshot};
use nsh_platform_tui::{AppOptions, AppView, CacheOptions, Input, Modifiers, Root};
use ratatui::{backend::TestBackend, Terminal};
use uuid::Uuid;

const COLS: u16 = 120;
const ROWS: u16 = 40;
const TIMEOUT: Duration = Duration::from_secs(10);
/// The composer's placeholder: on screen only when the Chat body is.
const CHAT_MARK: &str = "Message";

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
    fn wait_for(&mut self, what: &str, done: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.draw();
            if done(&self.screen()) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}\n{}", self.screen());
            let wait = Duration::from_millis(50);
            let _ = self.wake.recv_timeout(wait);
        }
    }

    /// Routes a key the way the terminal host does: the link's filter first,
    /// then the NSH view.
    fn send(&mut self, input: Input) {
        if let Some(input) = self.host.filter(input) {
            self.view.handle(input);
        }
    }

    fn key(&mut self, name: &str) {
        self.send(key(name));
    }

    fn type_text(&mut self, text: &str) {
        for c in text.chars() {
            self.key(&c.to_string());
        }
    }

    fn push(&mut self, snapshot: &ViewModelSnapshot) {
        self.host.push(snapshot);
    }

    /// The next action the host sent to the driver (waits briefly, as the
    /// engine may still be processing the key).
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
    let modifiers = Modifiers { ctrl: true, ..Modifiers::default() };
    Input::Key { key: name.to_string(), modifiers, release: false }
}

const IDS: [u128; 3] = [0xa1, 0xb2, 0xc3];

fn meta(n: u128, title: &str, running: bool) -> SessionMetaWire {
    SessionMetaWire {
        id: Uuid::from_u128(n),
        title: title.into(),
        created_at: String::new(),
        updated_at: String::new(),
        message_count: 4,
        task_branch: None,
        task_state: running.then(|| "running".to_string()),
        working_dir: None,
    }
}

/// Three sessions in core order, one running; rows formatted by the core.
fn fixture() -> ViewModelSnapshot {
    let sessions = vec![
        meta(IDS[0], "router fix", true),
        meta(IDS[1], "Docs pass", false),
        meta(IDS[2], "triage", false),
    ];
    ViewModelSnapshot {
        connection_label: "connected".into(),
        model: "claude-opus-5".into(),
        provider: "anthropic".into(),
        status_line: "claude-opus-5 \u{b7} 0 tok \u{b7} ask".into(),
        approval_mode_label: "ask".into(),
        approval_mode: Some(ApprovalModeWire::Ask),
        effort_label: String::new(),
        phase_label: "build".into(),
        sandbox: "off".into(),
        context_label: "ctx 12% (24k/200k)".into(),
        session_rows: sessions.iter().map(|m| session_row(m, m.task_state.is_some())).collect(),
        sessions,
        ..Default::default()
    }
}

fn open_sessions(h: &mut Harness) {
    h.push(&fixture());
    h.wait_for("chat", |s| s.contains(CHAT_MARK));
    h.send(ctrl("o"));
    h.wait_for("sessions rows", |s| s.contains("router fix"));
    // Opening Sessions asks the core for a fresh list; the tests read it here.
    assert_eq!(h.action(), Some(UiAction::ListSessions));
}

#[test]
fn opening_sessions_shows_the_core_rows_and_refreshes_the_list() {
    let mut h = Harness::new();
    open_sessions(&mut h);
    let screen = h.screen();
    assert!(screen.contains("router fix \u{b7} running \u{b7} 4 messages"), "{screen}");
    assert!(screen.contains("Docs pass \u{b7} idle \u{b7} 4 messages"), "{screen}");
}

#[test]
fn typing_filters_the_rows_case_insensitively() {
    let mut h = Harness::new();
    open_sessions(&mut h);
    h.type_text("DOCS");
    h.wait_for("filter text", |s| s.contains("filter: DOCS") && !s.contains("router fix"));
    assert!(h.screen().contains("Docs pass"));
}

#[test]
fn esc_clears_the_filter_then_closes_the_screen() {
    let mut h = Harness::new();
    open_sessions(&mut h);
    h.type_text("zzz");
    h.wait_for("no match", |s| s.contains("no sessions match"));
    h.key("Escape");
    h.wait_for("filter cleared", |s| s.contains("router fix") && !s.contains("filter: zzz"));
    h.key("Escape");
    h.wait_for("chat again", |s| s.contains(CHAT_MARK) && !s.contains("filter:"));
}

#[test]
fn enter_attaches_the_selected_row_and_returns_to_chat() {
    let mut h = Harness::new();
    open_sessions(&mut h);
    h.key("ArrowDown");
    h.wait_for("second row selected", |s| s.contains("> Docs pass"));
    h.key("Enter");
    assert_eq!(h.action(), Some(UiAction::Attach(Uuid::from_u128(IDS[1]))));
    h.wait_for("chat again", |s| s.contains(CHAT_MARK) && !s.contains("filter:"));
}

#[test]
fn n_creates_a_session_and_returns_to_chat() {
    let mut h = Harness::new();
    open_sessions(&mut h);
    h.key("n");
    assert_eq!(h.action(), Some(UiAction::NewSession));
    h.wait_for("chat again", |s| s.contains(CHAT_MARK));
}

#[test]
fn settings_shows_the_core_labels_and_enter_on_approval_mode_sends_the_mode() {
    let mut h = Harness::new();
    h.push(&fixture());
    h.wait_for("chat", |s| s.contains(CHAT_MARK));
    h.send(key("F2"));
    h.wait_for("settings", |s| s.contains("approval mode: ask"));
    let screen = h.screen();
    for line in ["model: claude-opus-5", "provider: anthropic", "phase: build", "effort: unset"] {
        assert!(screen.contains(line), "missing {line}\n{screen}");
    }
    assert!(screen.contains("context: ctx 12% (24k/200k)"), "{screen}");
    assert!(screen.contains("compact"), "{screen}");
    h.key("ArrowDown");
    h.key("ArrowDown");
    // Enter opens the approval mode picker; typing filters it and Enter applies.
    h.key("Enter");
    h.wait_for("approval mode picker", |s| s.contains("yolo") && s.contains("deny"));
    h.type_text("yolo");
    h.key("Enter");
    assert_eq!(h.action(), Some(UiAction::SetApprovalMode(ApprovalModeWire::ApproveAll)));
}

#[test]
fn esc_closes_settings() {
    let mut h = Harness::new();
    h.push(&fixture());
    h.wait_for("chat", |s| s.contains(CHAT_MARK));
    h.send(key("F2"));
    h.wait_for("settings", |s| s.contains("approval mode"));
    h.key("Escape");
    h.wait_for("chat again", |s| s.contains(CHAT_MARK) && !s.contains("approval mode"));
}
