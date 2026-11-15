//! Headless tests for APP-3: the model picker save (Shift+S), attachments,
//! and the list screens. Same harness as tests/chat.rs.
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
use modron_mobile_core::{view::model_row, ConnectionState, ViewModelSnapshot};
use modron_protocol::ModelWire;
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

fn fixture() -> ViewModelSnapshot {
    ViewModelSnapshot {
        connection: ConnectionState::Connected,
        connection_label: "connected".into(),
        model: "claude-opus-5".into(),
        provider: "anthropic".into(),
        ..Default::default()
    }
}

const CHAT_MARK: &str = "Message";

/// The next action that matches `want`, skipping others (the picker sends
/// `ListModels` when it opens).
fn action_matching(h: &mut Harness, want: impl Fn(&UiAction) -> bool) -> Option<UiAction> {
    for _ in 0..8 {
        match h.action() {
            Some(action) if want(&action) => return Some(action),
            Some(_) => continue,
            None => return None,
        }
    }
    None
}

fn catalog() -> ViewModelSnapshot {
    let wire = |id: &str, provider: &str, available: bool| ModelWire {
        id: id.into(),
        provider: provider.into(),
        label: id.into(),
        context_window: None,
        available,
        tier: None,
    };
    let mut snapshot = fixture();
    snapshot.model = "claude-opus-5".into();
    snapshot.provider = "anthropic".into();
    snapshot.models = vec![
        model_row(&wire("claude-opus-5", "anthropic", true), "claude-opus-5", "anthropic"),
        model_row(&wire("gpt-5", "openai", true), "claude-opus-5", "anthropic"),
        model_row(&wire("gemini-3", "google", false), "claude-opus-5", "anthropic"),
    ];
    snapshot
}

/// Opens the model picker with `/model` and waits for its rows.
fn open_picker(h: &mut Harness) {
    h.push(&catalog());
    h.wait_for("chat", |s| s.contains(CHAT_MARK));
    h.type_text("/model");
    h.settle();
    h.send(key("Enter"));
    // The host applies the picker's open step when it drains actions.
    assert_eq!(action_matching(h, |a| *a == UiAction::ListModels), Some(UiAction::ListModels));
    h.wait_for("model picker", |s| s.contains("gpt-5"));
}

#[test]
fn shift_s_saves_the_highlighted_model_and_provider_as_the_default() {
    let mut h = Harness::new();
    open_picker(&mut h);
    h.send(key("ArrowDown"));
    h.send(key("S"));
    assert_eq!(
        action_matching(&mut h, |a| matches!(a, UiAction::SaveDefaultModel { .. })),
        Some(UiAction::SaveDefaultModel { model: "gpt-5".into(), provider: "openai".into() })
    );
}

#[test]
fn enter_changes_only_the_session_model() {
    let mut h = Harness::new();
    open_picker(&mut h);
    h.send(key("ArrowDown"));
    h.send(key("Enter"));
    assert_eq!(
        action_matching(&mut h, |a| matches!(a, UiAction::SelectModel(_))),
        Some(UiAction::SelectModel("gpt-5".into()))
    );
    assert_eq!(action_matching(&mut h, |a| matches!(a, UiAction::SaveDefaultModel { .. })), None);
}

#[test]
fn shift_s_on_an_unavailable_model_saves_nothing() {
    let mut h = Harness::new();
    open_picker(&mut h);
    h.send(key("ArrowDown"));
    h.send(key("ArrowDown"));
    h.send(key("S"));
    assert_eq!(action_matching(&mut h, |a| matches!(a, UiAction::SaveDefaultModel { .. })), None);
}

fn ctrl(name: &str) -> Input {
    let modifiers = Modifiers { ctrl: true, ..Modifiers::default() };
    Input::Key { key: name.to_string(), modifiers, release: false }
}

#[test]
fn ctrl_v_asks_for_a_paste_and_a_staged_image_shows_its_chip() {
    modron_app::chat::clear_staged();
    let mut h = Harness::new();
    let mut snapshot = fixture();
    snapshot.running = false;
    h.push(&snapshot);
    h.wait_for("chat", |s| s.contains(CHAT_MARK));
    h.send(ctrl("v"));
    assert_eq!(action_matching(&mut h, |a| *a == UiAction::Paste), Some(UiAction::Paste));

    modron_app::chat::stage(modron_app::chat::Staged {
        name: "clipboard.png".into(),
        bytes: 2048,
        wire: modron_mobile_core::Attachment::Image {
            media_type: "image/png".into(),
            data: String::new(),
        },
    });
    h.push(&snapshot);
    h.wait_for("chip", |s| s.contains("[img clipboard.png"));
    assert!(h.screen().contains("2.0 KB"), "{}", h.screen());
    h.send(key("Backspace"));
    assert_eq!(action_matching(&mut h, |a| *a == UiAction::Detach), Some(UiAction::Detach));
    modron_app::chat::clear_staged();
}
