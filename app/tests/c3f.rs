//! Headless tests for Packet C3f: the composer's slash suggestions, the help
//! screen, the model picker, the session delete confirmation, and the
//! sub-agent screen (steer and kill). Same harness as tests/nav.rs: NSH's
//! AppView rendered into ratatui's TestBackend (120x40), keys through the
//! host's filter first, as the terminal host sends them.

use modron_mobile_core::view::AgentRowView;
use std::{
    sync::{mpsc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use modron_app::{
    chat::{link, Host, UiAction},
    App, CLIENT,
};
use modron_mobile_core::{session_row, ApprovalModeWire, SessionMetaWire, ViewModelSnapshot};
use modron_protocol::RouterDefaults;
use modron_types::AgentState;
use modron_view::SwarmRow;
use nsh_platform_tui::{AppOptions, AppView, CacheOptions, Input, Modifiers, Root};
use ratatui::{backend::TestBackend, Terminal};
use uuid::Uuid;

const COLS: u16 = 120;
const ROWS: u16 = 40;
const TIMEOUT: Duration = Duration::from_secs(10);

/// The host's next action, polled for up to [`TIMEOUT`]: the action is
/// produced on the driver's side, so one read can race under load.
fn poll_action(mut next: impl FnMut() -> Option<UiAction>) -> Option<UiAction> {
    let deadline = std::time::Instant::now() + TIMEOUT;
    loop {
        if let Some(action) = next() {
            return Some(action);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
/// The composer's placeholder: on screen only when the Chat body is.
const CHAT_MARK: &str = "Message";

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
    fn key(&mut self, name: &str) {
        self.send(key(name));
    }

    fn ctrl_key(&mut self, name: &str) {
        self.send(ctrl(name));
    }

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
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.draw();
            if modron_app::chat::composer_text() == want {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for composer text {want:?}\n{}",
                self.screen()
            );
            let _ = self.wake.recv_timeout(Duration::from_millis(20));
        }
    }

    /// Types one key at a time. A key the composer gets waits until the
    /// engine has applied it, so Enter after the text reads the whole text.
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

    /// The next action the host sent to the driver (waits briefly, as the
    /// engine may still be processing the key). Navigation steps are applied
    /// by the host and never returned.
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

const IDS: [u128; 2] = [0xa1, 0xb2];
const AGENT: &str = "agent-review";

fn meta(n: u128, title: &str, running: bool) -> SessionMetaWire {
    SessionMetaWire {
        forked_from: None,
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

fn router() -> RouterDefaults {
    RouterDefaults {
        authed: vec!["anthropic".into()],
        recommended_cheap: Some("claude-haiku-5-5".into()),
        recommended_strong: Some("claude-opus-5".into()),
        cheap_fallback: vec![],
        strong_fallback: vec!["claude-sonnet-5".into()],
    }
}

fn agent(state: AgentState) -> AgentRowView {
    AgentRowView::from_row(&SwarmRow {
        id: AGENT.into(),
        label: "reviewer".into(),
        turns: 3,
        tokens: 1200,
        last_tool: Some("read".into()),
        done: None,
        started_ms: None,
        state,
    })
}

/// Two sessions, a running agent, and router defaults, in core order.
fn fixture(agents: Vec<AgentRowView>) -> ViewModelSnapshot {
    let sessions = vec![meta(IDS[0], "router fix", true), meta(IDS[1], "Docs pass", false)];
    ViewModelSnapshot {
        connection_label: "connected".into(),
        model: "claude-opus-5".into(),
        provider: "anthropic".into(),
        status_line: "claude-opus-5 \u{b7} 0 tok \u{b7} ask".into(),
        approval_mode_label: "ask".into(),
        approval_mode: Some(ApprovalModeWire::Ask),
        phase_label: "build".into(),
        sandbox: "off".into(),
        session_rows: sessions.iter().map(|m| session_row(m, m.task_state.is_some())).collect(),
        sessions,
        subagent_rows: agents.iter().map(|a| a.text.clone()).collect(),
        agents,
        router_defaults: Some(router()),
        ..Default::default()
    }
}

fn chat_with(agents: Vec<AgentRowView>) -> Harness {
    let mut h = Harness::new();
    h.push(&fixture(agents));
    h.wait_for("chat", |s| s.contains(CHAT_MARK));
    h
}

#[test]
fn typing_a_slash_shows_suggestions_and_esc_hides_them_keeping_the_text() {
    let mut h = chat_with(vec![]);
    h.type_text("/co");
    h.wait_for("compact suggestion", |s| s.contains("/compact - compact this session"));
    h.key("Escape");
    h.wait_for("suggestions hidden", |s| !s.contains("/compact - compact this session"));
    assert!(h.screen().contains("/co"), "the typed text stays\n{}", h.screen());
    // Esc hid the list, so it did not interrupt the run.
    assert_eq!(h.action(), None);
}

#[test]
fn enter_completes_a_partial_command_then_runs_the_complete_one() {
    let mut h = chat_with(vec![]);
    h.type_text("/co");
    h.wait_for("compact suggestion", |s| s.contains("/compact - compact this session"));
    // Enter on a partial word completes it; nothing is sent yet.
    h.key("Enter");
    assert_eq!(h.action(), None);
    // The completion reaches the composer on the next engine turn.
    h.wait_draft("/compact");
    // Enter on the complete word runs it.
    h.key("Enter");
    assert_eq!(h.action(), Some(UiAction::Compact));
}

#[test]
fn f1_and_a_question_mark_open_help_and_esc_closes_it() {
    let mut h = chat_with(vec![]);
    // Help has three tabs: General opens first, Tab steps to Commands and on to Shortcuts.
    h.key("F1");
    h.wait_for("help screen", |s| s.contains("General") && s.contains("Shortcuts"));
    h.key("Tab");
    h.wait_for("commands tab", |s| s.contains("/model [name]"));
    h.key("Tab");
    h.wait_for("shortcuts tab", |s| s.contains("ctrl+o"));
    h.key("Escape");
    h.wait_for("chat again", |s| s.contains(CHAT_MARK) && !s.contains("Shortcuts"));

    h.key("?");
    h.wait_for("help from the question mark", |s| s.contains("Shortcuts"));
    h.key("Escape");
    h.wait_for("chat after help", |s| s.contains(CHAT_MARK) && !s.contains("Shortcuts"));
}

#[test]
fn model_picker_filters_and_enter_switches_the_model() {
    let mut h = chat_with(vec![]);
    h.type_text("/model");
    h.key("Enter");
    assert_eq!(poll_action(|| h.action()), Some(UiAction::ListModels));
    h.wait_for("picker rows", |s| s.contains("claude-haiku-5-5 (recommended cheap)"));
    h.type_text("haiku");
    h.wait_for("filtered", |s| {
        s.contains("filter: haiku") && !s.contains("claude-opus-5 (recommended")
    });
    h.key("Enter");
    assert_eq!(h.action(), Some(UiAction::SelectModel("claude-haiku-5-5".into())));
    h.wait_for("chat again", |s| s.contains(CHAT_MARK) && !s.contains("filter:"));
}

#[test]
fn esc_clears_the_model_filter_before_closing_the_picker() {
    let mut h = chat_with(vec![]);
    h.type_text("/model");
    h.key("Enter");
    assert_eq!(poll_action(|| h.action()), Some(UiAction::ListModels));
    h.wait_for("picker", |s| s.contains("claude-opus-5 (current)"));
    h.type_text("zz");
    h.wait_for("no match", |s| s.contains("filter: zz"));
    h.key("Escape");
    h.wait_for("filter cleared", |s| {
        !s.contains("filter: zz") && s.contains("claude-opus-5 (current)")
    });
    h.key("Escape");
    h.wait_for("chat", |s| s.contains(CHAT_MARK) && !s.contains("filter:"));
}

#[test]
fn delete_asks_inline_then_y_sends_the_delete_and_n_cancels() {
    let mut h = chat_with(vec![]);
    h.ctrl_key("o");
    assert_eq!(h.action(), Some(UiAction::ListSessions));
    h.wait_for("sessions", |s| s.contains("router fix"));
    h.key("d");
    h.wait_for("confirmation", |s| s.contains("delete router fix? y / n"));
    h.key("n");
    h.wait_for("cancelled", |s| !s.contains("delete router fix?"));
    assert_eq!(h.action(), None);

    h.key("d");
    h.wait_for("confirmation again", |s| s.contains("delete router fix? y / n"));
    h.key("y");
    assert_eq!(h.action(), Some(UiAction::DeleteSession(Uuid::from_u128(IDS[0]))));
    assert_eq!(h.action(), Some(UiAction::ListSessions));
}

#[test]
fn the_agent_screen_steers_and_kills_with_confirmation() {
    let mut h = chat_with(vec![agent(AgentState::Running)]);
    h.ctrl_key("a");
    h.wait_for("agent screen", |s| s.contains("[reviewer] running"));
    h.type_text("focus on tests");
    h.wait_for("typed", |s| s.contains("focus on tests"));
    eprintln!("DBG\n{}", h.screen());
    h.key("Enter");
    assert_eq!(
        h.action(),
        Some(UiAction::SteerAgent { agent: AGENT.into(), text: "focus on tests".into() })
    );

    h.key("k");
    h.wait_for("kill confirmation", |s| s.contains("kill reviewer? y / n"));
    h.key("y");
    assert_eq!(h.action(), Some(UiAction::KillAgent(AGENT.into())));
}

#[test]
fn an_ended_agent_keeps_its_row_and_offers_no_kill() {
    let mut h = chat_with(vec![agent(AgentState::Running)]);
    h.ctrl_key("a");
    h.wait_for("agent screen", |s| s.contains("[reviewer] running"));
    h.push(&fixture(vec![agent(AgentState::Killed)]));
    h.wait_for("final row", |s| s.contains("killed (ended)"));
    h.key("k");
    assert_eq!(h.action(), None);
    assert!(!h.screen().contains("kill reviewer?"), "{}", h.screen());
    h.key("Escape");
    h.wait_for("chat", |s| s.contains(CHAT_MARK));
}

#[test]
fn ctrl_a_without_agents_does_nothing() {
    let mut h = chat_with(vec![]);
    h.ctrl_key("a");
    assert_eq!(h.action(), None);
    assert!(h.screen().contains(CHAT_MARK));
}
