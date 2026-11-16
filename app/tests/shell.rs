//! Headless tests for shell input (PARITY-D2 item 1): the transcript rows,
//! the `!` and `!!` submits and the help rows. Same harness as tests/chat.rs.
//! Headless tests for the chat screen: NSH AppView rendered into ratatui's
//! TestBackend (120x40). Snapshots are built from the client core's public
//! types: `ViewModelSnapshot` and `PendingApprovalView` are plain structs with
//! pub fields, `DECISION_LABELS` is the core's decision copy, and transcript
//! rows go through the core's own `entry_lines`.

use std::{
    sync::{mpsc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use modron_app::{
    chat::{link, Host, UiAction},
    App, CLIENT,
};
use modron_mobile_core::{entry_lines, ConnectionState, RenderedRow, ViewModelSnapshot};
use modron_view::TranscriptEntry;
use nsh_platform_tui::{AppOptions, AppView, CacheOptions, Input, Modifiers, Root};
use ratatui::{backend::TestBackend, Terminal};

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
    fn wait_for(&mut self, what: &str, done: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.draw();
            if done(&self.screen()) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}\n{}", self.screen());
            let due = self.view.next_frame_deadline().map_or(deadline, |d| d.min(deadline));
            let wait = due.saturating_duration_since(Instant::now()).min(Duration::from_millis(50));
            let _ = self.wake.recv_timeout(wait);
        }
    }

    /// Draws for a short while, so the engine applies what the host sent
    /// (a recalled entry arrives in the composer a frame after the key).
    fn settle(&mut self) {
        let end = Instant::now() + Duration::from_millis(300);
        while Instant::now() < end {
            self.draw();
            let _ = self.wake.recv_timeout(Duration::from_millis(20));
        }
    }

    /// Routes a key the way the real host does: the link's filter first,
    /// then the NSH view.
    fn send(&mut self, input: Input) {
        if let Some(input) = self.host.filter(input) {
            self.view.handle(input);
        }
    }

    fn type_text(&mut self, text: &str) {
        for c in text.chars() {
            self.send(key(&c.to_string()));
        }
    }

    fn push(&mut self, snapshot: &ViewModelSnapshot) {
        self.host.push(snapshot);
    }

    /// The next action the view sent. Submit is produced by the engine while
    /// it processes the key, so draw and wait for it, as a real host would.
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

fn row(entry: TranscriptEntry) -> RenderedRow {
    RenderedRow { lines: entry_lines(&entry), entry }
}

fn fixture() -> ViewModelSnapshot {
    ViewModelSnapshot {
        connection: ConnectionState::Connected,
        connection_label: "connected".into(),
        model: "claude-opus-5".into(),
        provider: "anthropic".into(),
        ..Default::default()
    }
}

fn shell_row(command: &str, lines: &[&str], private: bool) -> RenderedRow {
    row(TranscriptEntry::Shell {
        command: command.into(),
        lines: lines.iter().map(|l| (*l).to_string()).collect(),
        is_error: false,
        private,
    })
}

#[test]
fn shell_rows_render_the_command_header_and_their_output() {
    let mut h = Harness::new();
    let mut snapshot = fixture();
    snapshot.history.push(shell_row("ls", &["a.txt"], false));
    snapshot.history.push(shell_row("env", &["HOME=/x"], true));
    h.push(&snapshot);
    h.wait_for("shell rows", |s| s.contains("$ ls") && s.contains("HOME=/x"));
    let screen = h.screen();
    assert!(screen.contains("a.txt"), "{screen}");
    assert!(screen.contains(&format!("$ env [{}]", modron_view::PRIVATE_SHELL_MARK)), "{screen}");
}

#[test]
fn double_bang_submits_a_private_shell_action() {
    let mut h = Harness::new();
    let mut snapshot = fixture();
    snapshot.running = false;
    h.push(&snapshot);
    h.settle();
    h.type_text("!!ls");
    h.send(key("Enter"));
    assert_eq!(h.action(), Some(UiAction::Shell { command: "ls".into(), private: true }));
}

#[test]
fn help_lists_the_double_bang_prefix() {
    let mut h = Harness::new();
    h.push(&fixture());
    h.send(key("F1"));
    // The keys live on the Shortcuts tab (Tab twice from General).
    h.send(key("Tab"));
    h.send(key("Tab"));
    h.wait_for("help", |s| s.contains("privately"));
    assert!(h.screen().contains("!!"), "{}", h.screen());
}
