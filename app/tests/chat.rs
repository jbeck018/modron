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
    clock::Instant as Clock,
    App, CLIENT,
};
use modron_mobile_core::{
    entry_lines, ConnectionState, DecisionOption, PendingApprovalView, RenderedRow,
    ViewModelSnapshot, DECISION_LABELS,
};
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
    /// Returns true when the view got the key (the host did not consume it).
    fn send(&mut self, input: Input) -> bool {
        if let Some(input) = self.host.filter(input) {
            self.view.handle(input);
            true
        } else {
            false
        }
    }

    fn type_text(&mut self, text: &str) {
        for c in text.chars() {
            self.send(key(&c.to_string()));
        }
    }

    /// The prompt is taken to have shown a second ago, so the type-ahead
    /// window (300 ms) is closed for keys these tests send at once. The guard
    /// has its own tests below, with injected times.
    fn push(&mut self, snapshot: &ViewModelSnapshot) {
        self.host.push_at(snapshot, Clock::now() - Duration::from_secs(1));
    }

    /// Routes a key at the injected time `now`, as the host's filter does.
    /// Returns true when the view got the key (the host did not consume it).
    fn send_at(&mut self, input: Input, now: Clock) -> bool {
        if let Some(input) = self.host.filter_at(input, now) {
            self.view.handle(input);
            true
        } else {
            false
        }
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
        server_version: "0.1.3".into(),
        running: true,
        running_label: "running".into(),
        status_line: "claude-opus-5 \u{b7} 18.4k tok \u{b7} cache 71% \u{b7} ask".into(),
        model: "claude-opus-5".into(),
        provider: "anthropic".into(),
        history: vec![
            row(TranscriptEntry::Task { text: "fix the router crate".into() }),
            row(TranscriptEntry::Assistant { text: "Looking at the router now.".into() }),
            row(TranscriptEntry::Model { model: "claude-opus-5".into(), reason: "default".into() }),
        ],
        ..Default::default()
    }
}

fn pending(snapshot: &mut ViewModelSnapshot) {
    snapshot.pending_approval = Some(PendingApprovalView {
        request_id: "req-1".into(),
        tool: "edit".into(),
        args_summary: "crates/modron-router/src/lib.rs".into(),
        arguments: serde_json::json!({}),
        arguments_text: "{}".into(),
        options: DECISION_LABELS
            .iter()
            .map(|(decision, label)| DecisionOption { decision: *decision, label: (*label).into() })
            .collect(),
    });
}

#[test]
fn transcript_rows_render_in_order() {
    let mut h = Harness::new();
    let snapshot = fixture();
    h.push(&snapshot);
    h.wait_for("task row", |s| s.contains("fix the router crate"));
    let screen = h.screen();
    let wanted = [
        "\u{276f} fix the router crate",
        "Looking at the router now.",
        "model: claude-opus-5 (default)",
    ];
    let mut at = 0;
    for text in wanted {
        let found = screen[at..]
            .find(text)
            .unwrap_or_else(|| panic!("missing or out of order: {text:?}\n{screen}"));
        at += found + text.len();
    }
}

#[test]
fn status_strings_render_verbatim() {
    let mut h = Harness::new();
    h.push(&fixture());
    h.wait_for("status line", |s| {
        s.contains("claude-opus-5 \u{b7} 18.4k tok \u{b7} cache 71% \u{b7} ask")
    });
    let screen = h.screen();
    for text in ["connected", "anthropic", "running"] {
        assert!(screen.contains(text), "missing {text:?}\n{screen}");
    }
}

#[test]
fn pending_approval_shows_all_three_labels() {
    let mut h = Harness::new();
    let mut snapshot = fixture();
    pending(&mut snapshot);
    h.push(&snapshot);
    h.wait_for("approval labels", |s| {
        s.contains("allow once") && s.contains("allow all") && s.contains("deny")
    });
    assert!(h.screen().contains("edit"));
}

#[test]
fn typing_and_enter_submits_and_keeps_focus() {
    let mut h = Harness::new();
    h.push(&fixture());
    h.wait_for("transcript", |s| s.contains("fix the router crate"));
    h.type_text("hi");
    h.wait_for("typed hi", |s| s.contains("hi"));
    h.send(key("Enter"));
    assert_eq!(h.action(), Some(UiAction::Send("hi".into())));
    // Focus stayed in the composer: the next text lands in it.
    h.type_text("zq");
    h.wait_for("typed after submit", |s| s.contains("zq"));
}

#[test]
fn a_during_pending_approval_decides_option_one() {
    let mut h = Harness::new();
    let mut snapshot = fixture();
    pending(&mut snapshot);
    h.push(&snapshot);
    h.wait_for("approval", |s| s.contains("allow all"));
    h.send(key("a"));
    assert_eq!(h.action(), Some(UiAction::Decide(1)));
    h.send(key("y"));
    assert_eq!(h.action(), Some(UiAction::Decide(0)));
    h.send(key("n"));
    assert_eq!(h.action(), Some(UiAction::Decide(2)));
}

#[test]
fn escape_sends_interrupt() {
    let mut h = Harness::new();
    h.push(&fixture());
    h.wait_for("transcript", |s| s.contains("fix the router crate"));
    h.send(key("Escape"));
    assert_eq!(h.action(), Some(UiAction::Interrupt));
}

#[test]
fn esc_on_an_idle_composer_clears_the_text_and_sends_nothing() {
    let mut h = Harness::new();
    let mut snapshot = fixture();
    snapshot.running = false;
    h.push(&snapshot);
    h.wait_for("transcript", |s| s.contains("fix the router crate"));
    h.type_text("zz");
    h.wait_for("typed", |s| s.contains("zz"));
    h.send(key("Escape"));
    h.wait_for("cleared", |s| !s.contains("zz"));
    assert_eq!(h.action(), None);
}

#[test]
fn context_command_shows_the_snapshot_numbers_as_a_note() {
    let mut h = Harness::new();
    let mut snapshot = fixture();
    snapshot.running = false;
    snapshot.context_label = "ctx 41% (74k/180k)".into();
    snapshot.cumulative_tokens = 1234;
    snapshot.session_cache_pct = Some(33.0);
    h.push(&snapshot);
    h.wait_for("transcript", |s| s.contains("fix the router crate"));
    h.type_text("/context");
    h.wait_for("the command typed", |_| modron_app::chat::composer_text() == "/context");
    h.send(key("Enter"));
    // The host makes the note when it pumps the action; the action never
    // reaches the driver, so every pump returns nothing.
    let want = "context: ctx 41% (74k/180k) \u{b7} 1234 tok \u{b7} cache 33%";
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert_eq!(h.host.try_action(), None);
        h.draw();
        if h.screen().contains(want) {
            break;
        }
        assert!(Instant::now() < deadline, "timed out waiting for the note\n{}", h.screen());
        let _ = h.wake.recv_timeout(Duration::from_millis(20));
    }
}

#[test]
fn a_standing_goal_shows_in_the_header() {
    let mut h = Harness::new();
    let mut snapshot = fixture();
    snapshot.goal = "ship the parity packet".into();
    h.push(&snapshot);
    h.wait_for("goal", |s| s.contains("[goal: ship the parity packet]"));
    snapshot.goal = String::new();
    h.push(&snapshot);
    h.wait_for("goal cleared", |s| !s.contains("[goal:"));
}

#[test]
fn streaming_text_appears_and_clears() {
    let mut h = Harness::new();
    let mut snapshot = fixture();
    snapshot.streaming_text = "Migrating the router now".into();
    h.push(&snapshot);
    h.wait_for("streaming text", |s| s.contains("Migrating the router now"));
    snapshot.streaming_text = String::new();
    h.push(&snapshot);
    h.wait_for("streaming text cleared", |s| !s.contains("Migrating the router now"));
}

/// A live turn: two tool rows, the turn summary, one sub-agent row and a note.
fn live_activity(snapshot: &mut ViewModelSnapshot) {
    snapshot.turn_summary_line =
        Some("\u{23fa} 2 tools \u{b7} read \u{d7}1 \u{b7} bash \u{d7}1".into());
    snapshot.tool_lines =
        vec!["\u{2713} read \u{2014} 2.1 KB".into(), "\u{25cf} bash \u{2026}".into()];
    snapshot.subagent_rows =
        vec!["\u{23f5} [reviewer] running \u{b7} 1 turns \u{b7} 300 tok \u{b7} last: read".into()];
    snapshot.last_note = Some("compacted 12 events: older turns".into());
}

#[test]
fn live_tool_strip_swarm_rows_and_notes_render_verbatim() {
    let mut h = Harness::new();
    let mut snapshot = fixture();
    live_activity(&mut snapshot);
    h.push(&snapshot);
    h.wait_for("live activity", |s| s.contains("compacted 12 events"));
    let screen = h.screen();
    let wanted = [
        "\u{23fa} 2 tools \u{b7} read \u{d7}1 \u{b7} bash \u{d7}1",
        "\u{2713} read \u{2014} 2.1 KB",
        "\u{25cf} bash \u{2026}",
        "\u{23f5} [reviewer] running \u{b7} 1 turns \u{b7} 300 tok \u{b7} last: read",
        "compacted 12 events: older turns",
    ];
    let mut at = 0;
    for text in wanted {
        let found = screen[at..]
            .find(text)
            .unwrap_or_else(|| panic!("missing or out of order: {text:?}\n{screen}"));
        at += found + text.len();
    }
}

#[test]
fn turn_summary_and_tool_rows_leave_with_the_turn() {
    let mut h = Harness::new();
    let mut snapshot = fixture();
    live_activity(&mut snapshot);
    h.push(&snapshot);
    h.wait_for("live tool rows", |s| s.contains("bash"));
    snapshot.turn_summary_line = None;
    snapshot.tool_lines.clear();
    snapshot.subagent_rows.clear();
    h.push(&snapshot);
    h.wait_for("rows cleared", |s| !s.contains("2 tools"));
    assert!(!h.screen().contains("reviewer"));
}

#[test]
fn prompt_marker_takes_a_theme_colour() {
    let mut h = Harness::new();
    h.push(&fixture());
    h.wait_for("task row", |s| s.contains("fix the router crate"));
    let buf = h.term.backend().buffer();
    let cell =
        buf.content.iter().find(|c| c.symbol() == "\u{276f}").expect("prompt marker on screen");
    assert_ne!(cell.fg, ratatui::style::Color::Reset, "primary token not applied");
}

fn shifted(name: &str) -> Input {
    Input::Key {
        key: name.to_string(),
        modifiers: Modifiers { shift: true, ..Modifiers::default() },
        release: false,
    }
}

#[test]
fn shift_enter_adds_a_line_and_enter_submits_the_whole_draft() {
    let mut h = Harness::new();
    h.push(&fixture());
    h.wait_for("transcript", |s| s.contains("fix the router crate"));
    h.type_text("a");
    h.wait_for("typed a", |s| s.contains('a'));
    h.send(shifted("Enter"));
    assert_eq!(h.action(), None, "shift+enter must not submit");
    h.type_text("b");
    h.wait_for("typed b", |s| s.contains('b'));
    h.send(key("Enter"));
    assert_eq!(h.action(), Some(UiAction::Send("a\nb".into())));
}

#[test]
fn a_pasted_block_lands_whole_and_nothing_submits() {
    let mut h = Harness::new();
    h.push(&fixture());
    h.wait_for("transcript", |s| s.contains("fix the router crate"));
    h.send(Input::Paste("one\ntwo\nthree".into()));
    assert_eq!(h.action(), None, "a paste must not submit");
    h.wait_for("pasted lines", |s| s.contains("one") && s.contains("three"));
    h.send(key("Enter"));
    assert_eq!(h.action(), Some(UiAction::Send("one\ntwo\nthree".into())));
}

#[test]
fn a_trailing_backslash_continues_the_line_and_does_not_submit() {
    let mut h = Harness::new();
    h.push(&fixture());
    h.wait_for("transcript", |s| s.contains("fix the router crate"));
    h.type_text("first\\");
    h.wait_for("typed the backslash", |s| s.contains("first\\"));
    h.send(key("Enter"));
    assert_eq!(h.action(), None, "a backslash line must not submit");
    h.type_text("second");
    h.wait_for("typed second", |s| s.contains("second"));
    h.send(key("Enter"));
    assert_eq!(h.action(), Some(UiAction::Send("first\nsecond".into())));
}

#[test]
fn up_recalls_the_last_sent_input_and_down_restores_the_draft() {
    let mut h = Harness::new();
    h.push(&fixture());
    h.wait_for("transcript", |s| s.contains("fix the router crate"));
    h.type_text("one");
    h.wait_for("typed one", |s| s.contains("one"));
    h.send(key("Enter"));
    assert_eq!(h.action(), Some(UiAction::Send("one".into())));
    h.send(key("ArrowUp"));
    h.settle();
    h.send(key("Enter"));
    assert_eq!(h.action(), Some(UiAction::Send("one".into())), "recalled text submits");
    h.settle();
    h.type_text("dr");
    h.wait_for("typed draft", |s| s.contains("dr"));
    h.send(key("ArrowUp"));
    h.settle();
    h.send(key("ArrowDown"));
    h.settle();
    h.send(key("Enter"));
    assert_eq!(h.action(), Some(UiAction::Send("dr".into())));
}

#[test]
fn esc_denies_a_pending_approval_instead_of_interrupting() {
    let mut h = Harness::new();
    let mut snapshot = fixture();
    pending(&mut snapshot);
    h.push(&snapshot);
    h.wait_for("approval", |s| s.contains("allow all"));
    h.send(key("Escape"));
    assert_eq!(h.action(), Some(UiAction::Decide(2)));
}

#[test]
fn paging_keys_scroll_without_touching_the_composer() {
    let mut h = Harness::new();
    h.push(&fixture());
    h.wait_for("transcript", |s| s.contains("fix the router crate"));
    h.send(key("PageUp"));
    h.send(key("PageDown"));
    h.send(key("Home"));
    h.send(key("End"));
    assert_eq!(h.action(), None, "paging is the host's, not an action");
    h.type_text("z");
    h.wait_for("typed after paging", |s| s.contains('z'));
    h.send(key("Enter"));
    assert_eq!(h.action(), Some(UiAction::Send("z".into())));
}

/// A fixture with one pending approval, named `id` (a queued second approval
/// is the same prompt with another request id).
fn approval_named(id: &str) -> ViewModelSnapshot {
    let mut snapshot = fixture();
    pending(&mut snapshot);
    snapshot.pending_approval.as_mut().expect("approval").request_id = id.into();
    snapshot
}

const GUARD_MS: u64 = 300;

#[test]
fn a_decision_key_inside_the_window_is_ignored_and_the_same_key_after_it_decides() {
    let mut h = Harness::new();
    let shown = Clock::now();
    h.host.push_at(&approval_named("req-1"), shown);
    h.wait_for("approval", |s| s.contains("allow all"));
    // Typed right as the prompt appears: consumed (the view never sees it),
    // and no decision is sent.
    let early = shown + Duration::from_millis(100);
    assert!(!h.send_at(key("a"), early), "the guard must consume the key");
    assert!(!h.send_at(key("Escape"), shown + Duration::from_millis(GUARD_MS - 1)));
    assert_eq!(h.host.try_action(), None);
    // The prompt is still up, and the key pressed again decides it.
    assert!(!h.send_at(key("a"), shown + Duration::from_millis(GUARD_MS)));
    assert_eq!(h.action(), Some(UiAction::Decide(1)));
}

#[test]
fn a_queued_approval_gets_its_own_window_and_a_repush_does_not_restart_it() {
    let mut h = Harness::new();
    let first = Clock::now();
    h.host.push_at(&approval_named("req-1"), first);
    h.wait_for("first approval", |s| s.contains("allow all"));
    // req-1 is answered a second later; req-2 becomes the front prompt.
    let second = first + Duration::from_secs(1);
    h.host.push_at(&approval_named("req-2"), second);
    assert!(!h.send_at(key("y"), second + Duration::from_millis(100)));
    // The same request id pushed again (a new snapshot) does not restart it:
    // the window still closes 300 ms after req-2 first showed.
    h.host.push_at(&approval_named("req-2"), second + Duration::from_millis(200));
    assert!(!h.send_at(key("y"), second + Duration::from_millis(GUARD_MS)));
    assert_eq!(h.action(), Some(UiAction::Decide(0)));
}

#[test]
fn a_new_prompt_restarts_the_window_even_when_the_last_one_was_old() {
    let mut h = Harness::new();
    let first = Clock::now();
    h.host.push_at(&approval_named("req-1"), first);
    h.wait_for("first approval", |s| s.contains("allow all"));
    assert!(!h.send_at(key("n"), first + Duration::from_secs(5)));
    assert_eq!(h.action(), Some(UiAction::Decide(2)));
    // A later prompt with a fresh id, shown at 10 s: a key at 10.1 s is ignored.
    let later = first + Duration::from_secs(10);
    h.host.push_at(&approval_named("req-3"), later);
    assert!(!h.send_at(key("y"), later + Duration::from_millis(100)));
    assert_eq!(h.host.try_action(), None);
}

#[test]
fn plan_keys_follow_the_same_window() {
    let mut h = Harness::new();
    let mut snapshot = fixture();
    snapshot.plan_pending = Some(modron_mobile_core::PendingPlanView::new(&[]));
    let shown = Clock::now();
    h.host.push_at(&snapshot, shown);
    h.wait_for("plan", |_| true);
    assert!(!h.send_at(key("y"), shown + Duration::from_millis(100)));
    assert!(!h.send_at(key("r"), shown + Duration::from_millis(200)));
    assert_eq!(h.host.try_action(), None);
    assert!(!h.send_at(key("y"), shown + Duration::from_millis(GUARD_MS)));
    assert_eq!(h.action(), Some(UiAction::DecidePlan { approve: true, note: None }));
}

#[test]
fn ordinary_typing_during_the_window_still_reaches_the_composer() {
    let mut h = Harness::new();
    let shown = Clock::now();
    h.host.push_at(&approval_named("req-1"), shown);
    h.wait_for("approval", |s| s.contains("allow all"));
    assert!(h.send_at(key("x"), shown + Duration::from_millis(10)));
}

// Untested DONE rows (docs/APP_PARITY.md, "Untested DONE rows"), app level.

#[test]
fn left_and_right_move_the_caret_and_typing_lands_there() {
    let mut h = Harness::new();
    h.push(&fixture());
    h.wait_for("transcript", |s| s.contains("fix the router crate"));
    h.type_text("ab");
    h.wait_for("typed ab", |_| modron_app::chat::composer_text() == "ab");
    h.send(key("ArrowLeft"));
    h.type_text("x");
    h.wait_for("x inserted before b", |_| modron_app::chat::composer_text() == "axb");
    h.send(key("ArrowRight"));
    h.type_text("y");
    h.wait_for("y at the end", |_| modron_app::chat::composer_text() == "axby");
}

#[test]
fn delete_removes_the_character_under_the_caret() {
    let mut h = Harness::new();
    h.push(&fixture());
    h.wait_for("transcript", |s| s.contains("fix the router crate"));
    h.type_text("abc");
    h.wait_for("typed abc", |_| modron_app::chat::composer_text() == "abc");
    h.send(key("ArrowLeft"));
    h.send(key("Delete"));
    h.wait_for("c deleted", |_| modron_app::chat::composer_text() == "ab");
}

#[test]
fn up_and_down_move_the_caret_between_lines_of_a_draft() {
    let mut h = Harness::new();
    h.push(&fixture());
    h.wait_for("transcript", |s| s.contains("fix the router crate"));
    h.type_text("a");
    h.send(shifted("Enter"));
    h.type_text("b");
    h.wait_for("two lines", |_| modron_app::chat::composer_text() == "a\nb");
    // Up from the end of "b" lands at column 1 of the first line (after "a").
    h.send(key("ArrowUp"));
    h.type_text("x");
    h.wait_for("x after a on the first line", |_| modron_app::chat::composer_text() == "ax\nb");
    // Down from column 2 of the first line lands at the end of "b".
    h.send(key("ArrowDown"));
    h.type_text("z");
    h.wait_for("z after b on the second line", |_| modron_app::chat::composer_text() == "ax\nbz");
}

#[test]
fn clear_hides_the_transcript_rows_on_screen_and_sends_nothing() {
    let mut h = Harness::new();
    let mut idle = fixture();
    idle.running = false;
    h.push(&idle);
    h.wait_for("transcript", |s| s.contains("fix the router crate"));
    h.type_text("/clear");
    h.wait_for("the command typed", |_| modron_app::chat::composer_text() == "/clear");
    h.send(key("Enter"));
    // Clear is applied by the host while it drains its actions: nothing is
    // left for the driver.
    assert_eq!(h.action(), None, "/clear is local: nothing is sent");
    h.wait_for("rows hidden", |s| !s.contains("fix the router crate"));
}

#[test]
fn exit_asks_the_host_to_quit() {
    let mut h = Harness::new();
    let mut idle = fixture();
    idle.running = false;
    h.push(&idle);
    h.wait_for("transcript", |s| s.contains("fix the router crate"));
    assert!(!modron_app::chat::quit_requested());
    h.type_text("/exit");
    h.wait_for("command typed", |_| modron_app::chat::composer_text() == "/exit");
    h.send(key("Enter"));
    assert_eq!(h.action(), None, "/exit is applied by the host");
    assert!(modron_app::chat::quit_requested());
}

#[test]
fn the_context_gauge_shows_the_warning_mark_at_eighty_percent() {
    // The core formats the status line, the gauge and its 80 percent warning
    // (view::context_label, status_line); the app shows the line verbatim.
    let mut h = Harness::new();
    let mut snapshot = fixture();
    snapshot.status_line = "claude-opus-5 · 153k tok · ctx 85%! (153k/180k) · ask".into();
    h.push(&snapshot);
    h.wait_for("the warning gauge in the status line", |s| s.contains("ctx 85%! (153k/180k)"));
    let mut calm = fixture();
    calm.status_line = "claude-opus-5 · 74k tok · ctx 41% (74k/180k) · ask".into();
    h.push(&calm);
    h.wait_for("the calm gauge", |s| s.contains("ctx 41% (74k/180k)"));
    assert!(!h.screen().contains("41%!"));
}
