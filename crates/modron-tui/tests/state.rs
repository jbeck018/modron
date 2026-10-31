//! Phase 1d TUI tests: the App state machine (pure) and the live-region
//! rendering via ratatui's TestBackend. The terminal loop itself needs a
//! real TTY and is intentionally thin.

use modron_core::{StreamDelta, ToolActivity, ToolCall, Usage};
use modron_tui::{Action, App, PendingApproval};
use serde_json::json;
use std::sync::mpsc;

fn line_text(line: &ratatui::text::Line) -> String {
    line.spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect::<String>()
}

fn key(code: crossterm::event::KeyCode) -> crossterm::event::KeyEvent {
    crossterm::event::KeyEvent::from(code)
}

fn ctrl(code: crossterm::event::KeyCode) -> crossterm::event::KeyEvent {
    crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::CONTROL)
}

fn pending() -> PendingApproval {
    let (tx, _rx) = mpsc::channel();
    PendingApproval {
        call: ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments: json!({"command": "rm -rf /"}),
        },
        respond: tx,
    }
}

#[test]
fn typing_and_enter_submits_the_input() {
    let mut app = App::default();
    for c in "fix the bug".chars() {
        assert_eq!(
            app.on_key(key(crossterm::event::KeyCode::Char(c))),
            Action::None
        );
    }
    assert_eq!(app.input, "fix the bug");
    assert_eq!(
        app.on_key(key(crossterm::event::KeyCode::Enter)),
        Action::Submit("fix the bug".into())
    );
    assert_eq!(app.input, "");
}

#[test]
fn esc_clears_input_when_idle_and_aborts_when_running() {
    let mut app = App::default();
    app.input.push_str("partial");
    assert_eq!(
        app.on_key(key(crossterm::event::KeyCode::Esc)),
        Action::None
    );
    assert!(app.input.is_empty(), "idle esc clears the input");

    let mut running = App {
        running: true,
        ..App::default()
    };
    assert_eq!(
        running.on_key(key(crossterm::event::KeyCode::Esc)),
        Action::Abort
    );
}

#[test]
fn ctrl_c_quits_when_idle_and_aborts_when_running() {
    let mut idle = App::default();
    assert_eq!(
        idle.on_key(ctrl(crossterm::event::KeyCode::Char('c'))),
        Action::Quit
    );
    let mut running = App {
        running: true,
        ..App::default()
    };
    assert_eq!(
        running.on_key(ctrl(crossterm::event::KeyCode::Char('c'))),
        Action::Abort
    );
}

#[test]
fn ctrl_o_toggles_transcript_mode_in_any_state() {
    let mut idle = App::default();
    assert_eq!(
        idle.on_key(ctrl(crossterm::event::KeyCode::Char('o'))),
        Action::None
    );
    assert!(idle.transcript_mode);
    let mut running = App {
        running: true,
        ..App::default()
    };
    assert_eq!(
        running.on_key(ctrl(crossterm::event::KeyCode::Char('o'))),
        Action::None
    );
    assert!(running.transcript_mode);
}

#[test]
fn approval_keys_respond_and_the_sticky_a_is_recorded() {
    let (tx, rx) = mpsc::channel::<bool>();
    let mut app = App {
        pending_approval: Some(PendingApproval {
            call: ToolCall {
                id: "c".into(),
                name: "bash".into(),
                arguments: json!({"command": "ls"}),
            },
            respond: tx,
        }),
        ..App::default()
    };
    assert_eq!(
        app.on_key(key(crossterm::event::KeyCode::Char('a'))),
        Action::None
    );
    assert!(rx.recv().unwrap(), "`a` allows");
    assert!(app.approve_rest, "sticky approve-rest recorded");
    assert!(app.pending_approval.is_none());

    let (tx2, rx2) = mpsc::channel();
    app.pending_approval = Some(PendingApproval {
        call: ToolCall {
            id: "c2".into(),
            name: "write".into(),
            arguments: json!({"path": "x"}),
        },
        respond: tx2,
    });
    assert_eq!(
        app.on_key(key(crossterm::event::KeyCode::Esc)),
        Action::None
    );
    assert!(!rx2.recv().unwrap(), "Esc denies");
}

#[test]
fn non_decision_keys_do_not_swallow_a_pending_approval() {
    let (_tx, rx) = mpsc::channel::<bool>();
    let mut app = App {
        pending_approval: Some(pending()),
        ..App::default()
    };
    // Typing while the approval is up must NOT dismiss it.
    assert_eq!(
        app.on_key(key(crossterm::event::KeyCode::Char('x'))),
        Action::None
    );
    assert!(app.pending_approval.is_some(), "approval stays pending");
    assert!(rx.try_recv().is_err(), "no decision sent");
    // And the input did not change either.
    assert!(app.input.is_empty());
}

#[test]
fn tool_lines_render_start_and_done() {
    let mut app = App::default();
    app.on_tool(ToolActivity::Start {
        name: "bash".into(),
    });
    app.on_tool(ToolActivity::Done {
        name: "bash".into(),
        is_error: false,
        bytes: 42,
    });
    assert_eq!(app.tool_lines.len(), 2);
    assert!(app.tool_lines[0].contains("● bash …"));
    assert!(app.tool_lines[1].contains("done (42 B)"));
    assert!(app.current_tool.is_none());
}

#[test]
fn finish_turn_flushes_tools_and_answer_and_updates_totals() {
    let mut app = App::default();
    app.on_tool(ToolActivity::Done {
        name: "bash".into(),
        is_error: false,
        bytes: 10,
    });
    app.on_delta(StreamDelta::Text("partial answer".into()));
    let id = modron_core::SessionId::new_v4();
    let lines = app.finish_turn(
        "the full answer".into(),
        Usage {
            input_tokens: 100,
            output_tokens: 20,
            cached_input_tokens: 0,
        },
        2,
        1,
        id,
    );
    assert!(
        lines.iter().any(|l| line_text(l).contains("● bash")),
        "tool lines flush: {lines:?}"
    );
    assert!(lines.iter().any(|l| line_text(l) == "the full answer"));
    assert_eq!(app.cumulative.total(), 120);
    assert!(app.session_id == Some(id));
    assert!(!app.running);
    assert!(
        app.streaming.is_empty(),
        "streaming cleared for the next turn"
    );
}

#[test]
fn streaming_tail_is_bounded_and_wraps_manually() {
    let text = "x".repeat(1000);
    let tail = modron_tui::streaming_tail(&text, 5);
    assert_eq!(tail.len(), 5, "window bounded");
    for line in &tail {
        assert!(line.width() <= 110, "manual wrap keeps lines short");
    }
    // Multi-byte safety: emoji do not split.
    let emoji = "🦀".repeat(300);
    let tail = modron_tui::streaming_tail(&emoji, 3);
    assert_eq!(tail.len(), 3);
}

fn render(app: &App, width: u16, height: u16) -> String {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| app.view(frame, frame.area()))
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            out.push(buffer[(x, y)].symbol().chars().next().unwrap_or(' '));
        }
        out.push('\n');
    }
    out
}

#[test]
fn idle_view_renders_prompt_and_status() {
    let app = App {
        model_label: "glm-5.3".into(),
        input: "hello world".into(),
        ..App::default()
    };
    let frame = render(&app, 80, 6);
    assert!(
        frame.contains("❯ hello world"),
        "input in the bordered box: {frame:?}"
    );
    assert!(frame.contains("glm-5.3"), "model in status: {frame:?}");
    assert!(frame.contains("╭"), "rounded border: {frame:?}");
    assert!(frame.contains("▎"), "cursor block: {frame:?}");
}

#[test]
fn approval_view_renders_the_prompt_and_hint() {
    let app = App {
        pending_approval: Some(pending()),
        ..App::default()
    };
    let frame = render(&app, 80, 7);
    assert!(frame.contains("allow bash"), "approval prompt: {frame:?}");
    assert!(frame.contains("y = allow once"), "decision hint: {frame:?}");
    assert!(
        frame.contains("rm -rf /"),
        "the dangerous command is visible: {frame:?}"
    );
    assert!(frame.contains("╭"), "input box stays rendered: {frame:?}");
}

#[test]
fn running_view_shows_spinner_and_interrupt_hint() {
    let mut app = App {
        running: true,
        model_label: "glm-5.3".into(),
        ..App::default()
    };
    app.on_delta(StreamDelta::Text("streaming answer text".into()));
    let frame = render(&app, 80, 8);
    assert!(frame.contains("esc interrupt"), "interrupt hint: {frame:?}");
    assert!(
        frame.contains("streaming answer text"),
        "streaming tail: {frame:?}"
    );
    assert!(
        frame.contains("working"),
        "working title on the border: {frame:?}"
    );
}
