//! Phase 1d TUI tests: the App state machine (pure) and the live-region
//! rendering via ratatui's TestBackend. The terminal loop itself needs a
//! real TTY and is intentionally thin.

use modron_core::{StreamDelta, ToolActivity, ToolCall, Usage};
use modron_tui::{Action, App, AuthFlow, PendingApproval, PendingCommand, SlashCommand};
use serde_json::json;
use std::sync::mpsc;

fn history_text(app: &App) -> Vec<String> {
    app.history
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect()
}

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
    // Grouped view: events accumulate; the scrollback stays clean.
    assert!(history_text(&app).is_empty(), "no per-tool history lines");
    assert_eq!(app.turn_tools.len(), 1);
    assert!(app.turn_tools[0].done);
    assert!(app.turn_tools[0].summary().contains("bash"));
    assert!(app.turn_tools[0].summary().contains("42 B"));
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
    // The turn's tools land as ONE grouped summary line before the answer.
    assert!(
        history_text(&app)
            .iter()
            .any(|l| l.contains("1 tool · bash")),
        "grouped summary in history: {:?}",
        history_text(&app)
    );
    assert_eq!(app.tool_log.len(), 1, "logged for the expandable panel");
    assert!(app.turn_tools.is_empty(), "cleared for the next turn");
    assert!(lines.iter().any(|l| line_text(l) == "the full answer"));
    assert!(
        history_text(&app).iter().any(|l| l == "the full answer"),
        "answer committed to history"
    );
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
    let tail = modron_tui::streaming_tail(&text, 5, 110);
    assert_eq!(tail.len(), 5, "window bounded");
    for line in &tail {
        assert!(line.width() <= 110, "manual wrap keeps lines short");
    }
    // Multi-byte safety: emoji do not split.
    let emoji = "🦀".repeat(300);
    let tail = modron_tui::streaming_tail(&emoji, 3, 110);
    assert_eq!(tail.len(), 3);
}

fn render(app: &mut App, width: u16, height: u16) -> String {
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
    let mut app = App {
        model_label: "glm-5.3".into(),
        input: "hello world".into(),
        cursor: "hello world".chars().count(),
        ..App::default()
    };
    let frame = render(&mut app, 80, 6);
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
    let mut app = App {
        pending_approval: Some(pending()),
        ..App::default()
    };
    let frame = render(&mut app, 80, 7);
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
    let frame = render(&mut app, 80, 8);
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

#[test]
fn slash_commands_parse_and_route() {
    assert_eq!(SlashCommand::parse("/help"), Some(SlashCommand::Help));
    assert_eq!(SlashCommand::parse("/exit"), Some(SlashCommand::Exit));
    assert_eq!(SlashCommand::parse("/q"), Some(SlashCommand::Exit));
    assert_eq!(
        SlashCommand::parse("/model glm-5.3-flash"),
        Some(SlashCommand::Model("glm-5.3-flash".into()))
    );
    assert_eq!(
        SlashCommand::parse("/bogus"),
        Some(SlashCommand::Unknown("bogus".into()))
    );
    assert_eq!(SlashCommand::parse("regular task"), None);
}

#[test]
fn slash_commands_are_handled_locally() {
    let mut app = App::default();
    // /help opens the tabbed help modal (handled locally, never submitted)
    app.on_key(key(crossterm::event::KeyCode::Char('/')));
    for c in "help".chars() {
        app.on_key(key(crossterm::event::KeyCode::Char(c)));
    }
    let action = app.on_key(key(crossterm::event::KeyCode::Enter));
    assert_eq!(action, Action::None, "help is local, not submitted");
    assert!(app.help.is_some(), "the tabbed help modal is open");
    assert!(
        history_text(&app).is_empty(),
        "the modal replaces the old scrollback dump: {:?}",
        history_text(&app)
    );

    // /exit quits
    let mut app2 = App::default();
    app2.input.push_str("/exit");
    let action = app2.on_key(key(crossterm::event::KeyCode::Enter));
    assert_eq!(action, Action::Quit);

    // /model switches
    let mut app3 = App::default();
    app3.input.push_str("/model glm-5.3");
    app3.on_key(key(crossterm::event::KeyCode::Enter));
    assert_eq!(app3.model_label, "glm-5.3");
}

#[test]
fn context_meter_renders_the_bar() {
    let mut app = App {
        context_window: 100_000,
        context_used: 50_000,
        ..App::default()
    };
    let meter = app.context_meter();
    assert!(meter.contains("50%"), "{meter}");
    assert!(meter.contains("▓"), "bar characters: {meter}");

    app.context_used = 85_000;
    let meter = app.context_meter();
    assert!(meter.contains("!compact"), "compaction warning: {meter}");
}

// ── Multi-line input (newline paths) ──────────────────────────────────────

fn with_modifiers(
    code: crossterm::event::KeyCode,
    mods: crossterm::event::KeyModifiers,
) -> crossterm::event::KeyEvent {
    crossterm::event::KeyEvent::new(code, mods)
}

#[test]
fn shift_enter_and_alt_enter_insert_newlines_not_submit() {
    for (name, mods) in [
        ("shift", crossterm::event::KeyModifiers::SHIFT),
        ("alt", crossterm::event::KeyModifiers::ALT),
    ] {
        let mut app = App::default();
        for c in "line one".chars() {
            app.on_key(key(crossterm::event::KeyCode::Char(c)));
        }
        assert_eq!(
            app.on_key(with_modifiers(crossterm::event::KeyCode::Enter, mods)),
            Action::None,
            "{name}+enter must not submit"
        );
        for c in "line two".chars() {
            app.on_key(key(crossterm::event::KeyCode::Char(c)));
        }
        assert_eq!(app.input, "line one\nline two", "{name}+enter");
        // Plain enter still submits the whole multi-line task.
        assert_eq!(
            app.on_key(key(crossterm::event::KeyCode::Enter)),
            Action::Submit("line one\nline two".into())
        );
    }
}

#[test]
fn ctrl_j_inserts_a_newline() {
    // Ctrl+J = raw LF: what a terminal sends when shift+enter is remapped
    // to "\n" (the standard iTerm2 workaround).
    let mut app = App::default();
    for c in "top".chars() {
        app.on_key(key(crossterm::event::KeyCode::Char(c)));
    }
    assert_eq!(
        app.on_key(ctrl(crossterm::event::KeyCode::Char('j'))),
        Action::None
    );
    for c in "bottom".chars() {
        app.on_key(key(crossterm::event::KeyCode::Char(c)));
    }
    assert_eq!(app.input, "top\nbottom");
}

#[test]
fn backslash_enter_continues_the_line() {
    let mut app = App::default();
    for c in "first\\".chars() {
        app.on_key(key(crossterm::event::KeyCode::Char(c)));
    }
    assert_eq!(
        app.on_key(key(crossterm::event::KeyCode::Enter)),
        Action::None,
        "backslash+enter must not submit"
    );
    assert_eq!(app.input, "first\n");
    for c in "second".chars() {
        app.on_key(key(crossterm::event::KeyCode::Char(c)));
    }
    assert_eq!(
        app.on_key(key(crossterm::event::KeyCode::Enter)),
        Action::Submit("first\nsecond".into())
    );
}

#[test]
fn multi_line_task_echoes_as_indented_block() {
    let mut app = App::default();
    app.begin_task("one\ntwo\nthree");
    assert_eq!(history_text(&app), vec!["❯ one", "  two", "  three"]);
}

// ── Slash command expansion (Phase 14) ────────────────────────────────────

#[test]
fn new_slash_commands_parse_with_aliases() {
    assert_eq!(
        SlashCommand::parse("/compress"),
        Some(SlashCommand::Compact)
    );
    assert_eq!(
        SlashCommand::parse("/review"),
        Some(SlashCommand::CodeReview)
    );
    assert_eq!(
        SlashCommand::parse("/code-review"),
        Some(SlashCommand::CodeReview)
    );
    assert_eq!(
        SlashCommand::parse("/reasoning"),
        Some(SlashCommand::Reasoning)
    );
    assert_eq!(
        SlashCommand::parse("/thinking"),
        Some(SlashCommand::Reasoning)
    );
    assert_eq!(
        SlashCommand::parse("/btw what is this?"),
        Some(SlashCommand::Btw("what is this?".into()))
    );
    assert_eq!(
        SlashCommand::parse("/allow bash"),
        Some(SlashCommand::Allow("bash".into()))
    );
    assert_eq!(
        SlashCommand::parse("/goal tests pass"),
        Some(SlashCommand::Goal("tests pass".into()))
    );
}

#[test]
fn reasoning_toggles_and_btw_submits_no_tools_rider() {
    let mut app = App::default();
    assert!(!app.show_reasoning);
    assert_eq!(
        app.on_key(key(crossterm::event::KeyCode::Enter)),
        Action::None
    );
    // /reasoning via direct handler
    assert_eq!(app.handle_slash_test(SlashCommand::Reasoning), Action::None);
    assert!(app.show_reasoning);

    let mut app = App::default();
    match app.handle_slash_test(SlashCommand::Btw("how big is it?".into())) {
        Action::Submit(task) => {
            assert!(task.contains("how big is it?"));
            assert!(task.contains("do not use tools"), "{task}");
        }
        other => panic!("btw must submit: {other:?}"),
    }
}

#[test]
fn plan_and_review_submit_constrained_tasks() {
    let mut app = App::default();
    match app.handle_slash_test(SlashCommand::Plan("add auth".into())) {
        Action::Submit(task) => assert!(task.contains("Do NOT edit any files"), "{task}"),
        other => panic!("plan must submit: {other:?}"),
    }
    match app.handle_slash_test(SlashCommand::CodeReview) {
        Action::Submit(task) => assert!(task.contains("git diff"), "{task}"),
        other => panic!("review must submit: {other:?}"),
    }
}

#[test]
fn goal_sets_clears_and_rides_along_on_tasks() {
    let mut app = App::default();
    app.handle_slash_test(SlashCommand::Goal("all tests green".into()));
    assert_eq!(app.goal.as_deref(), Some("all tests green"));
    let ridden = app.task_with_goal("fix the flaky test");
    assert!(ridden.starts_with("fix the flaky test"), "{ridden}");
    assert!(ridden.contains("all tests green"), "{ridden}");
    assert!(ridden.contains("GOAL COMPLETE"), "{ridden}");
    // begin_task echoes the goal line.
    app.begin_task(&ridden);
    assert!(history_text(&app)
        .iter()
        .any(|l| l.contains("[goal: all tests green]")));
    // Bare /goal clears.
    app.handle_slash_test(SlashCommand::Goal(String::new()));
    assert!(app.goal.is_none());
    assert_eq!(app.task_with_goal("next"), "next");
}

#[test]
fn learn_submits_a_skill_distillation_task() {
    let mut app = App::default();
    match app.handle_slash_test(SlashCommand::Learn("deploy flow".into())) {
        Action::Submit(task) => {
            assert!(
                task.contains(".modron/skills/deploy-flow/SKILL.md"),
                "{task}"
            );
            assert!(task.contains("description:"), "{task}");
        }
        other => panic!("learn must submit: {other:?}"),
    }
}

#[test]
fn engine_powered_commands_park_as_pending() {
    let mut app = App::default();
    for (cmd, pending) in [
        (
            SlashCommand::Agent("reviewer".into()),
            PendingCommand::Agent("reviewer".into()),
        ),
        (
            SlashCommand::Allow("bash".into()),
            PendingCommand::Allow("bash".into()),
        ),
        (
            SlashCommand::Deny("write".into()),
            PendingCommand::Deny("write".into()),
        ),
        (
            SlashCommand::Mode("yolo".into()),
            PendingCommand::Mode("yolo".into()),
        ),
        (SlashCommand::Tasks, PendingCommand::Tasks),
        (SlashCommand::Worktree, PendingCommand::Worktree),
        (
            SlashCommand::Issues("add flaky test".into()),
            PendingCommand::Issues("add flaky test".into()),
        ),
        (SlashCommand::Fork, PendingCommand::Fork),
    ] {
        app.pending_command = None;
        assert_eq!(app.handle_slash_test(cmd), Action::None);
        assert_eq!(
            app.pending_command.as_ref(),
            Some(&pending) as Option<&PendingCommand>
        );
    }
}

#[test]
fn reasoning_deltas_display_only_when_enabled() {
    use modron_core::StreamDelta;
    let mut app = App::default();
    app.on_delta(StreamDelta::Reasoning("chain".into()));
    assert!(app.reasoning.is_empty(), "hidden by default");
    app.show_reasoning = true;
    app.on_delta(StreamDelta::Reasoning("chain of".into()));
    app.on_delta(StreamDelta::Text("answer".into()));
    assert_eq!(app.reasoning, "chain of");
    assert_eq!(app.streaming, "answer");
}

// ── Theme system (Phase 14) ────────────────────────────────────────────────

#[test]
fn theme_slash_opens_menu_and_switches() {
    let mut app = App::default();
    assert_eq!(app.theme.name, "modron");
    // Bare /theme opens the interactive picker (built-ins listed).
    app.handle_slash_test(SlashCommand::Theme(String::new()));
    let menu = app.menu.as_ref().expect("theme menu");
    let labels: Vec<&str> = menu.options.iter().map(|o| o.label.as_str()).collect();
    assert!(labels.iter().any(|l| l.contains("midnight")), "{labels:?}");
    assert!(labels.iter().any(|l| l.contains("mono")), "{labels:?}");
    // Selecting an option through the menu switches the theme.
    let target = menu
        .options
        .iter()
        .find(|o| o.label.contains("midnight"))
        .map(|o| o.label.trim_start_matches("● ").to_string())
        .unwrap();
    let index = menu
        .options
        .iter()
        .position(|o| o.label.contains("midnight"))
        .unwrap();
    for _ in 0..index {
        app.on_key(key(crossterm::event::KeyCode::Down));
    }
    app.on_key(key(crossterm::event::KeyCode::Enter));
    assert!(app.menu.is_none(), "menu closes on selection");
    assert_eq!(app.theme.name, target);
    assert_eq!(app.current_theme, target);
    // Typed switch still works (muscle memory path).
    app.handle_slash_test(SlashCommand::Theme("midnight".into()));
    assert_eq!(app.theme.name, "midnight");
    assert_eq!(
        app.theme.resolved().accent,
        ratatui::style::Color::Rgb(122, 162, 247)
    );
    app.handle_slash_test(SlashCommand::Theme("no-such-theme".into()));
    assert_eq!(app.theme.name, "midnight", "unknown theme does not switch");
    assert!(
        history_text(&app)
            .iter()
            .any(|l| l.contains("unknown theme")),
        "{:?}",
        history_text(&app)
    );
}

// ── Interactive command menus (bare arg-commands open pickers) ─────────────

#[test]
fn model_menu_lists_catalog_and_switches_the_engine_model() {
    use crossterm::event::KeyCode;
    let mut app = App {
        current_model: "glm-5.3".into(),
        ..App::default()
    };

    app.handle_slash_test(SlashCommand::Model(String::new()));
    let menu = app.menu.as_ref().expect("model menu");
    assert!(
        menu.options.len() >= 10,
        "whole catalog listed: {}",
        menu.options.len()
    );
    assert!(
        menu.options
            .iter()
            .any(|o| o.label.contains("glm-5.3-flash")),
        "{:?}",
        menu.options.iter().map(|o| &o.label).collect::<Vec<_>>()
    );
    // The current model is marked and preselected.
    assert!(
        menu.options[menu.selected].label.contains("●"),
        "current model preselected: {:?}",
        menu.options[menu.selected].label
    );

    // Enter re-dispatches the selected model: the engine switch is parked
    // (the label alone would lie about taking effect).
    app.on_key(key(KeyCode::Enter));
    assert!(app.menu.is_none());
    assert!(matches!(
        app.pending_command,
        Some(PendingCommand::Model { ref name, .. }) if name == "glm-5.3"
    ));

    // Esc closes without side effects.
    let mut app2 = App::default();
    app2.handle_slash_test(SlashCommand::Model(String::new()));
    app2.on_key(key(KeyCode::Esc));
    assert!(app2.menu.is_none());
    assert_eq!(app2.pending_command, None);
}

#[test]
fn sandbox_and_mode_menus_park_pending_commands() {
    use crossterm::event::KeyCode;
    let mut app = App::default();
    app.handle_slash_test(SlashCommand::Sandbox(String::new()));
    assert!(app.menu.as_ref().unwrap().options.len() == 3);
    app.on_key(key(KeyCode::Down)); // seatbelt
    app.on_key(key(KeyCode::Enter));
    assert!(matches!(
        app.pending_command,
        Some(PendingCommand::Sandbox(ref m)) if m == "seatbelt"
    ));

    app.handle_slash_test(SlashCommand::Mode(String::new()));
    app.on_key(key(KeyCode::Down)); // yolo
    app.on_key(key(KeyCode::Enter));
    assert!(matches!(
        app.pending_command,
        Some(PendingCommand::Mode(ref m)) if m == "yolo"
    ));
}

#[test]
fn persona_and_tool_menus_list_injected_names() {
    use crossterm::event::KeyCode;
    let mut app = App {
        personas: vec!["reviewer".into(), "explorer".into()],
        tool_names: vec!["bash".into(), "read".into()],
        ..App::default()
    };

    app.handle_slash_test(SlashCommand::Agent(String::new()));
    assert_eq!(app.menu.as_ref().unwrap().options.len(), 2);
    app.on_key(key(KeyCode::Down));
    app.on_key(key(KeyCode::Enter));
    assert!(matches!(
        app.pending_command,
        Some(PendingCommand::Agent(ref n)) if n == "explorer"
    ));

    app.handle_slash_test(SlashCommand::Allow(String::new()));
    assert_eq!(app.menu.as_ref().unwrap().options.len(), 2);
    app.on_key(key(KeyCode::Enter));
    assert!(matches!(
        app.pending_command,
        Some(PendingCommand::Allow(ref t)) if t == "bash"
    ));

    app.handle_slash_test(SlashCommand::Deny(String::new()));
    app.on_key(key(KeyCode::Enter));
    assert!(matches!(
        app.pending_command,
        Some(PendingCommand::Deny(ref t)) if t == "bash"
    ));

    // Empty persona registry: no empty menu — a plain note explains.
    let mut bare = App::default();
    bare.handle_slash_test(SlashCommand::Agent(String::new()));
    assert!(bare.menu.is_none());
    assert_eq!(bare.pending_command, None);
    assert!(
        history_text(&bare)
            .iter()
            .any(|l| l.contains("no personas configured")),
        "{:?}",
        history_text(&bare)
    );
}

#[test]
fn model_menu_renders_with_detail_column() {
    let mut app = App::default();
    app.handle_slash_test(SlashCommand::Model(String::new()));
    let frame = render(&mut app, 110, 24);
    assert!(frame.contains("model —"), "title: {frame:?}");
    assert!(frame.contains("menu open"), "placeholder: {frame:?}");
    assert!(
        frame.contains(" · ") || frame.contains("k"),
        "provider/window detail: {frame:?}"
    );
}

// ── Swarm view v2 (Phase 14) ───────────────────────────────────────────────

#[test]
fn swarm_rows_track_lifecycle_and_summarize() {
    use modron_tui::SwarmRow;
    let mut app = App::default();
    app.on_tool(ToolActivity::SubagentStarted {
        id: "a1".into(),
        label: "reviewer".into(),
    });
    app.on_tool(ToolActivity::SubagentProgress {
        id: "a1".into(),
        label: "reviewer".into(),
        turns: 3,
        tokens: 1200,
        last_tool: Some("grep".into()),
    });
    // Wrong id is ignored.
    app.on_tool(ToolActivity::SubagentProgress {
        id: "zz".into(),
        label: String::new(),
        turns: 9,
        tokens: 9,
        last_tool: None,
    });
    app.on_tool(ToolActivity::SubagentFinished {
        id: "a1".into(),
        label: "reviewer".into(),
        tokens: 1400,
        turns: 4,
        is_error: false,
    });
    let row: &SwarmRow = app.subagents.first().unwrap();
    assert_eq!((row.turns, row.tokens, row.done), (4, 1400, Some(true)));
    assert_eq!(row.last_tool.as_deref(), Some("grep"));

    let summary = app.swarm_summary();
    assert!(!summary.is_empty());
    assert!(app.subagents.is_empty(), "summary clears the panel");
    // Summary lines render as spans; join their text.
    let text: String = summary
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("swarm: 1 agent(s)"), "{text}");
    assert!(text.contains("[reviewer] done"), "{text}");
    assert!(text.contains("1400 tok"), "{text}");
    assert!(text.contains("last: grep"), "{text}");
}

#[test]
fn swarm_summary_empty_when_no_agents_ran() {
    let mut app = App::default();
    assert!(app.swarm_summary().is_empty());
}

#[test]
fn slash_popup_filters_matches_from_the_catalog() {
    let mut app = App::default();
    assert!(app.slash_matches().is_empty(), "no popup without a /");

    app.input.push('/');
    app.cursor = 1;
    assert_eq!(
        app.slash_matches().len(),
        SlashCommand::CATALOG.len(),
        "bare / lists every command"
    );

    for c in "mod".chars() {
        app.on_key(key(crossterm::event::KeyCode::Char(c)));
    }
    let names: Vec<&str> = app.slash_matches().iter().map(|(n, _)| *n).collect();
    assert_eq!(names, vec!["model", "mode"], "prefix filter, catalog order");

    app.input = "/model glm".into();
    assert!(app.slash_matches().is_empty(), "space closes the popup");

    app.input = "/zzz".into();
    assert!(app.slash_matches().is_empty(), "no matches, no popup");
}

#[test]
fn catalog_names_are_unique_and_all_parse() {
    let mut names: Vec<&str> = SlashCommand::CATALOG.iter().map(|(n, _)| *n).collect();
    let total = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), total, "duplicate catalog entry");

    for (name, _) in SlashCommand::CATALOG {
        match SlashCommand::parse(&format!("/{name}")) {
            Some(SlashCommand::Unknown(_)) => {
                panic!("/{name} parses as Unknown — catalog out of sync with parse()")
            }
            None => panic!("/{name} does not parse"),
            Some(_) => {}
        }
    }
}

#[test]
fn tab_completes_and_arrows_move_the_slash_popup() {
    use crossterm::event::KeyCode;
    // Unique match: tab completes it with a trailing space for arguments.
    let mut app = App::default();
    app.input.push_str("/mo");
    assert_eq!(app.on_key(key(KeyCode::Tab)), Action::None);
    assert_eq!(app.input, "/model ");

    // Several matches: down moves the highlight, tab takes the selected one.
    // /c matches clear, compact, compress, context, code-review (in order).
    let mut app2 = App::default();
    app2.input.push_str("/c");
    app2.cursor = 2;
    assert_eq!(app2.on_key(key(KeyCode::Down)), Action::None);
    assert_eq!(app2.on_key(key(KeyCode::Down)), Action::None);
    assert_eq!(app2.on_key(key(KeyCode::Tab)), Action::None);
    assert_eq!(app2.input, "/compress ", "two downs select the third match");

    // Up wraps around to the bottom of the list.
    let mut app3 = App::default();
    app3.input.push_str("/c");
    app3.cursor = 2;
    assert_eq!(app3.on_key(key(KeyCode::Up)), Action::None);
    assert_eq!(app3.on_key(key(KeyCode::Tab)), Action::None);
    assert_eq!(
        app3.input, "/code-review ",
        "up from the top wraps to the last"
    );

    // Typing after navigating resets the highlight to the top.
    let mut app4 = App::default();
    app4.input.push_str("/c");
    app4.cursor = 2;
    app4.on_key(key(KeyCode::Down));
    app4.on_key(key(KeyCode::Char('l')));
    assert_eq!(app4.on_key(key(KeyCode::Tab)), Action::None);
    assert_eq!(app4.input, "/clear ");

    // Arrows are inert when the popup is closed.
    let mut app5 = App::default();
    app5.input.push_str("plain text");
    assert_eq!(app5.on_key(key(KeyCode::Down)), Action::None);
    assert_eq!(app5.input, "plain text");
}

#[test]
fn slash_popup_renders_above_the_input_box() {
    let mut app = App {
        input: "/c".into(),
        cursor: 2,
        ..App::default()
    };
    let frame = render(&mut app, 80, 10);
    assert!(frame.contains("❯ /clear"), "top row highlighted: {frame:?}");
    assert!(frame.contains("/compact"), "middle rows listed: {frame:?}");
    assert!(
        frame.contains("/code-review"),
        "all five /c rows fit: {frame:?}"
    );
    assert!(frame.contains("❯ /c"), "input echo survives: {frame:?}");
    assert!(frame.contains("╭"), "input border survives: {frame:?}");

    // A space after the command word closes the popup.
    let mut closed = App {
        input: "/model glm".into(),
        cursor: 10,
        ..App::default()
    };
    let frame = render(&mut closed, 80, 10);
    assert!(!frame.contains("/clear"), "no popup with args: {frame:?}");
}

#[test]
fn cursor_model_edits_at_the_cursor_not_the_end() {
    use crossterm::event::KeyCode;
    let mut app = App::default();
    for c in "hello".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    assert_eq!(app.cursor, 5, "typing advances the cursor");

    // Left twice, then type: insertion happens mid-text.
    app.on_key(key(KeyCode::Left));
    app.on_key(key(KeyCode::Left));
    assert_eq!(app.cursor, 3);
    app.on_key(key(KeyCode::Char('X')));
    assert_eq!(app.input, "helXlo");

    // Backspace deletes before the cursor; Delete deletes after it.
    app.on_key(key(KeyCode::Backspace));
    assert_eq!(app.input, "hello");
    assert_eq!(app.cursor, 3);
    app.on_key(key(KeyCode::Left));
    app.on_key(key(KeyCode::Delete));
    assert_eq!(app.input, "helo");

    // Home/End move within the current line only.
    let mut app2 = App {
        input: "one\ntwo".into(),
        cursor: 5, // on "two", after the 't'
        ..App::default()
    };
    app2.on_key(key(KeyCode::End));
    assert_eq!(app2.cursor, 7, "end of the second line");
    app2.on_key(key(KeyCode::Home));
    assert_eq!(app2.cursor, 4, "home of the second line, not of the input");

    // Up/Down move between input lines, keeping the column.
    app2.on_key(key(KeyCode::Up));
    assert_eq!(app2.cursor, 0, "col 0 on line 1");
    app2.on_key(key(KeyCode::End));
    app2.on_key(key(KeyCode::Down));
    assert_eq!(app2.cursor, 7, "col 3 clamped to line 2's end");

    // Right clamps at the end.
    let mut app3 = App {
        input: "ab".into(),
        ..App::default()
    };
    for _ in 0..5 {
        app3.on_key(key(KeyCode::Right));
    }
    assert_eq!(app3.cursor, 2);
}

#[test]
fn click_positions_the_cursor_inside_the_input_box() {
    let mut app = App {
        input: "one\ntwo".into(),
        ..App::default()
    };
    // Simulate geometry recorded by the last render: inner at (1, 5), 50x4.
    app.last_input_inner = ratatui::layout::Rect {
        x: 1,
        y: 5,
        width: 50,
        height: 4,
    };

    // Click on row 6 (second text line), column 10 → "two" col 7 clamped.
    app.on_click(10, 6);
    assert_eq!(app.cursor, 7, "click past the line end clamps to it");

    // Click on the first line, column 4 → col 1 of "one".
    app.on_click(4, 5);
    assert_eq!(app.cursor, 1);

    // Click below the text rows → cursor to the end.
    app.on_click(4, 8);
    assert_eq!(app.cursor, 7);

    // Click outside the box → unchanged.
    app.on_click(4, 2);
    assert_eq!(app.cursor, 7);

    // Clicking the chips row (first inner row) puts the cursor at the end.
    app.attachment_chips = vec!["[img x.png · 1 KiB]".into()];
    app.on_click(4, 5);
    assert_eq!(app.cursor, 7);
}

#[test]
fn multiline_input_renders_the_cursor_on_the_typed_line() {
    let mut app = App::default();
    app.input = "first line here\nsecond line here".into();
    app.cursor = app.input.chars().count(); // end of the second line
    let frame = render(&mut app, 80, 10);
    assert!(
        frame.contains("second line here▎"),
        "cursor sits right after the second line: {frame:?}"
    );
    assert!(
        !frame.contains("first line here  ▎"),
        "no stray cursor padding on the first line: {frame:?}"
    );

    // Cursor mid-first-line renders there instead.
    app.cursor = 5;
    let frame = render(&mut app, 80, 10);
    assert!(frame.contains("first▎"), "cursor mid-line: {frame:?}");
}

#[test]
fn long_multiline_input_scrolls_to_keep_the_cursor_visible() {
    let mut app = App::default();
    app.input = (1..=8)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.cursor = app.input.chars().count(); // end of "line 8"
    let frame = render(&mut app, 80, 12);
    assert!(frame.contains("line 8"), "last line visible: {frame:?}");
    assert!(frame.contains("line 8▎"), "cursor on it: {frame:?}");
    assert!(!frame.contains("line 1\n"), "first line scrolled out");
}

#[test]
fn internal_scrollback_pages_and_follows() {
    use crossterm::event::KeyCode;
    let mut app = App::default();
    for i in 0..50 {
        app.note(format!("line {i}"));
    }
    app.last_history_height = 10;

    // Following the tail: the last lines are the window.
    let frame = render(&mut app, 80, 16);
    assert!(frame.contains("line 49"), "tail visible: {frame:?}");
    assert!(!frame.contains("line 0\n"), "top scrolled out");

    // PageUp scrolls up a page (the render set the page height to 12:
    // 16 rows - 1 status - 3 input); the indicator appears.
    assert_eq!(app.on_key(key(KeyCode::PageUp)), Action::None);
    assert_eq!(app.history_offset, 12);
    let frame = render(&mut app, 80, 16);
    assert!(frame.contains("lines above"), "scroll indicator: {frame:?}");
    assert!(!frame.contains("line 49\n"), "tail left the window");

    // New content while scrolled does NOT move the view.
    app.note("line 50");
    assert_eq!(app.history_offset, 12, "anchored while scrolled");

    // PageDown beyond the bottom clamps to live; Esc then clears the input.
    app.on_key(key(KeyCode::PageDown));
    assert_eq!(app.history_offset, 0);
    app.on_key(key(KeyCode::PageDown)); // clamps at live
    app.input = "leftover".into();
    app.cursor = 8;
    app.on_key(key(KeyCode::Esc));
    assert!(app.input.is_empty(), "esc clears input once live");

    // Scrolled-up Esc jumps to live instead of clearing.
    app.on_key(key(KeyCode::PageUp));
    app.input = "keep me".into();
    app.cursor = 7;
    app.on_key(key(KeyCode::Esc));
    assert_eq!(app.input, "keep me", "input kept on the jump-to-live esc");
    assert_eq!(app.history_offset, 0);

    // Wheel steps (scroll_history) clamp at both ends.
    app.scroll_history(999);
    assert_eq!(app.history_offset, 50);
    app.scroll_history(-999);
    assert_eq!(app.history_offset, 0);
}

#[test]
fn streaming_renders_as_live_tail_in_the_history_zone() {
    let mut app = App {
        running: true,
        model_label: "glm-5.3".into(),
        ..App::default()
    };
    app.note("committed line");
    app.on_delta(StreamDelta::Text("streaming answer text".into()));
    let frame = render(&mut app, 80, 10);
    assert!(frame.contains("committed line"), "history shows: {frame:?}");
    assert!(
        frame.contains("streaming answer text"),
        "live tail below history: {frame:?}"
    );
    assert!(frame.contains("working"), "working title: {frame:?}");
}

#[test]
fn auth_slash_bare_opens_the_manager_args_still_park() {
    let mut app = App::default();
    // Bare /auth (and /auth add) open the interactive manager.
    app.handle_slash_test(SlashCommand::Auth(String::new()));
    assert!(matches!(
        app.auth_flow,
        Some(AuthFlow::Picker { selected: 0 })
    ));
    assert_eq!(app.pending_command, None);
    let mut app = App::default();
    app.handle_slash_test(SlashCommand::Auth("add".into()));
    assert!(matches!(
        app.auth_flow,
        Some(AuthFlow::Picker { selected: 0 })
    ));

    // Arg forms still park the direct command.
    let mut app = App::default();
    app.handle_slash_test(SlashCommand::Auth("groq gsk-abc".into()));
    assert_eq!(
        app.pending_command,
        Some(PendingCommand::Auth("groq gsk-abc".into()))
    );
    app.handle_slash_test(SlashCommand::Auth("status".into()));
    assert_eq!(
        app.pending_command,
        Some(PendingCommand::Auth("status".into()))
    );

    // /login is an alias that parses to the same command, args included.
    assert_eq!(
        SlashCommand::parse("/login"),
        Some(SlashCommand::Auth(String::new())),
        "catalog advertises /login as an alias; parse must match"
    );
    assert_eq!(
        SlashCommand::parse("/auth set zai sk-1"),
        Some(SlashCommand::Auth("set zai sk-1".into()))
    );
}

#[test]
fn auth_wizard_flows_picker_to_saved_key() {
    use crossterm::event::KeyCode;
    let mut app = App::default();
    app.handle_slash_test(SlashCommand::Auth(String::new()));
    assert!(matches!(
        app.auth_flow,
        Some(AuthFlow::Picker { selected: 0 })
    ));

    // Registry order puts subscriptions first (anthropic, openai-compat,
    // zai, google, groq): groq sits at index 4.
    for _ in 0..4 {
        app.on_key(key(KeyCode::Down));
    }
    app.on_key(key(KeyCode::Enter));
    match &app.auth_flow {
        Some(AuthFlow::MethodChoice { info, .. }) => assert_eq!(info.provider_id, "groq"),
        other => panic!("expected method choice, got {other:?}"),
    }
    // API key is the first method for key-only providers.
    app.on_key(key(KeyCode::Enter));
    match &app.auth_flow {
        Some(AuthFlow::KeyEntry { info, .. }) => assert_eq!(info.provider_id, "groq"),
        other => panic!("expected key entry, got {other:?}"),
    }

    // Esc from key entry goes BACK to the provider list (same provider
    // highlighted), not out of the manager.
    app.on_key(key(KeyCode::Esc));
    match &app.auth_flow {
        Some(AuthFlow::Picker { selected }) => assert_eq!(*selected, 4),
        other => panic!("esc should return to the picker, got {other:?}"),
    }
    app.on_key(key(KeyCode::Enter));
    app.on_key(key(KeyCode::Enter));

    // Typed chars land in the wizard buffer, never the main input.
    for c in "gk-live-secret".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    app.on_key(key(KeyCode::Backspace));
    assert!(app.input.is_empty(), "main input untouched");

    // Enter saves: parks AuthSave (masked later by the run loop) and
    // closes the modal.
    app.on_key(key(KeyCode::Enter));
    assert!(app.auth_flow.is_none());
    match &app.pending_command {
        Some(PendingCommand::AuthSave { name, env_var, key }) => {
            assert_eq!(env_var, "GROQ_API_KEY");
            assert_eq!(key, "gk-live-secre");
            assert!(name.contains("Groq"));
        }
        other => panic!("expected AuthSave, got {other:?}"),
    }
    // The raw key never renders into the scrollback.
    assert!(
        !history_text(&app).iter().any(|l| l.contains("secret")),
        "{:?}",
        history_text(&app)
    );

    // Esc at the picker closes the manager without side effects.
    let mut app2 = App::default();
    app2.handle_slash_test(SlashCommand::Auth(String::new()));
    app2.on_key(key(KeyCode::Esc));
    assert!(app2.auth_flow.is_none());
    assert!(app2.pending_command.is_none());
}

#[test]
fn auth_wizard_browser_sign_in_parks_the_right_flow() {
    use crossterm::event::KeyCode;

    // Anthropic (index 0): method choice -> browser sign-in -> the PKCE
    // session is prepared and parked; the listener+exchange run spawned.
    let mut app = App::default();
    app.handle_slash_test(SlashCommand::Auth(String::new()));
    app.on_key(key(KeyCode::Enter));
    app.on_key(key(KeyCode::Down));
    app.on_key(key(KeyCode::Enter));
    assert!(matches!(
        app.auth_flow,
        Some(AuthFlow::WaitingOAuth { info: ref i, ref url, ref user_code })
            if i.provider_id == "anthropic" && !url.is_empty() && user_code.is_none()
    ));
    assert!(matches!(
        app.pending_command,
        Some(PendingCommand::OAuthLogin { .. })
    ));
    // Esc backs out to the method screen (the manager stays open; the
    // run loop aborts the spawned flow), and Esc again returns to the
    // provider list.
    app.on_key(key(KeyCode::Esc));
    assert!(matches!(
        app.auth_flow,
        Some(AuthFlow::MethodChoice { info: ref i, .. }) if i.provider_id == "anthropic"
    ));
    app.on_key(key(KeyCode::Esc));
    assert!(matches!(app.auth_flow, Some(AuthFlow::Picker { .. })));

    // OpenAI/ChatGPT (index 1): method choice -> browser sign-in -> the
    // device flow (URL + user code arrive from the spawned task).
    let mut app = App::default();
    app.handle_slash_test(SlashCommand::Auth(String::new()));
    app.on_key(key(KeyCode::Down));
    app.on_key(key(KeyCode::Enter));
    app.on_key(key(KeyCode::Down));
    app.on_key(key(KeyCode::Enter));
    assert!(matches!(
        app.auth_flow,
        Some(AuthFlow::WaitingOAuth { info: ref i, ref url, ref user_code })
            if i.provider_id == "openai-compat" && url.is_empty() && user_code.is_none()
    ));
    assert!(matches!(
        app.pending_command,
        Some(PendingCommand::ChatGptLogin)
    ));
    app.on_key(key(KeyCode::Esc));
    assert!(matches!(
        app.auth_flow,
        Some(AuthFlow::MethodChoice { info: ref i, .. }) if i.provider_id == "openai-compat"
    ));
}

#[test]
fn auth_wizard_remove_parks_and_esc_navigates_back() {
    use crossterm::event::KeyCode;
    let mut app = App::default();
    app.handle_slash_test(SlashCommand::Auth(String::new()));
    app.on_key(key(KeyCode::Enter)); // anthropic -> method choice
                                     // Methods for oauth-capable providers: api key, browser, remove.
    app.on_key(key(KeyCode::Down));
    app.on_key(key(KeyCode::Down));
    app.on_key(key(KeyCode::Enter));
    assert!(app.auth_flow.is_none());
    match &app.pending_command {
        Some(PendingCommand::AuthRemove {
            name,
            env_var,
            provider_id,
        }) => {
            assert_eq!(env_var, "ANTHROPIC_API_KEY");
            assert_eq!(provider_id, "anthropic");
            assert!(name.contains("Anthropic"));
        }
        other => panic!("expected AuthRemove, got {other:?}"),
    }

    // Esc from method choice returns to the picker highlighting the
    // provider we came from.
    let mut app2 = App::default();
    app2.handle_slash_test(SlashCommand::Auth(String::new()));
    app2.on_key(key(KeyCode::Down)); // openai
    app2.on_key(key(KeyCode::Enter));
    app2.on_key(key(KeyCode::Esc));
    match &app2.auth_flow {
        Some(AuthFlow::Picker { selected }) => assert_eq!(*selected, 1),
        other => panic!("esc should return to the picker, got {other:?}"),
    }
}

#[test]
fn auth_wizard_renders_manager_and_masks_the_key() {
    use crossterm::event::KeyCode;
    let mut app = App::default();
    app.handle_slash_test(SlashCommand::Auth(String::new()));
    let frame = render(&mut app, 90, 24);
    assert!(frame.contains("auth —"), "manager header: {frame:?}");
    assert!(frame.contains("Anthropic"), "subscriptions lead: {frame:?}");
    assert!(
        frame.contains("not configured"),
        "rows show status: {frame:?}"
    );

    // Bottom of the list is the last registry provider (the separate
    // OAuth rows are gone — they live inside method choice now).
    let reg = modron_core::auth::provider_registry().len();
    for _ in 0..(reg - 1) {
        app.on_key(key(KeyCode::Down));
    }
    let frame = render(&mut app, 90, 24);
    assert!(frame.contains("GitHub Models"), "{frame:?}");

    // Back to the top and into the method screen (api key first).
    for _ in 0..(reg - 1) {
        app.on_key(key(KeyCode::Up));
    }
    app.on_key(key(KeyCode::Enter)); // anthropic -> method choice
    let frame = render(&mut app, 90, 24);
    assert!(
        frame.contains("sign in with Claude subscription"),
        "browser sign-in offered: {frame:?}"
    );
    app.on_key(key(KeyCode::Enter)); // api key (first method) -> key entry
    for c in "sk-secret-value".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    let frame = render(&mut app, 90, 24);
    assert!(frame.contains("•••"), "masked dots: {frame:?}");
    assert!(
        !frame.contains("sk-secret-value"),
        "raw key never rendered: {frame:?}"
    );
    assert!(
        frame.contains("ANTHROPIC_API_KEY"),
        "names the env var: {frame:?}"
    );
    assert!(
        frame.contains("auth manager"),
        "input box signals the modal: {frame:?}"
    );
}

// ── Mouse text selection + copy ─────────────────────────────────────────────

#[test]
fn selection_extracts_partial_and_whole_lines() {
    use crossterm::event::KeyCode;
    let mut app = App::default();
    app.note("hello selection world");
    app.note("second line here");
    app.note("third");
    // Simulate the geometry the renderer records: 3 committed rows at the
    // top, following.
    app.hist_geom = (0, 3, true, 0, 0);

    // Drag across the middle of rows 0 and 1 (reversed direction too):
    // col 6 drops "hello ", col 11 on row 1 keeps "second line".
    app.begin_selection(6, 0);
    app.extend_selection(11, 1);
    assert_eq!(
        app.selected_text().as_deref(),
        Some("selection world\nsecond line"),
        "{:?}",
        app.selected_text()
    );
    // Reversed drag selects the same text.
    app.begin_selection(11, 1);
    app.extend_selection(6, 0);
    assert_eq!(
        app.selected_text().as_deref(),
        Some("selection world\nsecond line")
    );

    // Whole-line window.
    app.begin_selection(0, 1);
    app.extend_selection(99, 2);
    assert_eq!(
        app.selected_text().as_deref(),
        Some("second line here\nthird")
    );

    // take returns the text and clears; a second take yields nothing.
    assert!(app.take_selected_text().is_some());
    assert!(app.take_selected_text().is_none(), "already taken");
    app.begin_selection(0, 0);
    app.extend_selection(3, 0);
    assert!(app.selection.is_some());
    app.on_key(key(KeyCode::Esc));
    assert!(app.selection.is_none());

    // A click outside the history zone drops the selection.
    app.begin_selection(0, 0);
    app.extend_selection(3, 0);
    app.begin_selection(0, 90); // row 90 is outside history
    assert!(app.selection.is_none());
}

#[test]
fn selection_renders_reversed_rows() {
    let mut app = App::default();
    app.note("alpha beta gamma");
    app.hist_geom = (0, 1, true, 0, 0);
    app.begin_selection(6, 0);
    app.extend_selection(10, 0);
    // Rendered frame still contains the text (with the highlight applied).
    let frame = render(&mut app, 80, 24);
    assert!(frame.contains("alpha beta gamma"), "{frame:?}");
    assert!(
        app.selection.is_some(),
        "render must not drop the selection"
    );
}

#[test]
fn selection_maps_rows_through_scrolled_geometry() {
    let mut app = App::default();
    for i in 0..40 {
        app.note(format!("line-{i}"));
    }
    // Scrolled up one screen: indicator row on top, start at index 30.
    app.hist_geom = (30, 9, false, 0, 0);
    app.begin_selection(0, 1); // first committed row = index 30
    app.extend_selection(4, 2);
    assert_eq!(app.selected_text().as_deref(), Some("line-30\nline"));
}

#[test]
fn selection_counts_double_width_glyphs_as_two_columns() {
    let mut app = App::default();
    // Two CJK glyphs (4 screen columns) then ASCII.
    app.note("你好world");
    app.hist_geom = (0, 3, true, 0, 0);
    // Click at screen column 4 = after both glyphs = char offset 2;
    // drag to column 7 = char offset 5 -> "wor".
    app.begin_selection(4, 0);
    app.extend_selection(7, 0);
    assert_eq!(app.selected_text().as_deref(), Some("wor"));
    // Clicking the SECOND glyph's start (column 2) anchors at char 1;
    // column 5 = past 'w' (char 3) -> "好w".
    app.begin_selection(2, 0);
    app.extend_selection(5, 0);
    assert_eq!(app.selected_text().as_deref(), Some("好w"));
}

// ── Input overflow wrapping + sent-input history ────────────────────────────

#[test]
fn long_single_line_input_wraps_into_display_rows() {
    // 200 chars on ONE line (no newline): must overflow onto rows, not clip.
    let mut app = App {
        input: "a".repeat(200),
        cursor: 200,
        ..App::default()
    };
    let frame = render(&mut app, 80, 24);
    // The whole text renders across wrapped rows (border/status chars
    // share the frame, so check the full-width runs exist).
    assert!(frame.contains(&"a".repeat(76)), "full display rows present");
    let total: usize = frame.chars().filter(|c| *c == 'a').count();
    assert!(total >= 200, "every char rendered: {total}");

    // cursor_display maps through the wrap: the inner width is 78 and the
    // prompt/indent takes 2, so rows are 76 chars — char 150 sits on row 1
    // at column 74 (the cursor at the very end, 200, sits on the last row).
    app.cursor = 150;
    let (row, col) = app.cursor_display(78);
    assert_eq!(row, 1, "second row for char 150");
    assert_eq!(col as usize, 150 - 76, "column within the row");
    app.cursor = 200;
    let (row, _) = app.cursor_display(78);
    assert_eq!(row, 2, "last row for the end cursor");

    // The box height is capped and the view still renders (scrolling box).
    let frame = render(&mut app, 80, 10);
    assert!(!frame.is_empty());

    // Click maps through display rows: clicking row 1 col 0 puts the
    // cursor at the start of the second display row.
    let inner = app.last_input_inner;
    app.on_click(inner.x + 2, inner.y + 1);
    assert_eq!(app.cursor, 76, "click maps into wrapped rows");
}

#[test]
fn input_history_recall_traverses_and_restores_draft() {
    use crossterm::event::KeyCode;
    let mut app = App::default();
    // Submit two tasks (Enter parks Submit actions).
    app.input = "first task".into();
    app.cursor = app.input.chars().count();
    let _ = app.on_key(key(KeyCode::Enter));
    app.input = "second task".into();
    app.cursor = app.input.chars().count();
    let _ = app.on_key(key(KeyCode::Enter));
    assert_eq!(app.input_history.len(), 2);

    // Typing a draft, then Up recalls the newest; Up again the older one.
    for c in "draft".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    app.on_key(key(KeyCode::Up));
    assert_eq!(app.input, "second task");
    app.on_key(key(KeyCode::Up));
    assert_eq!(app.input, "first task");
    // Down walks forward; past the newest restores the draft.
    app.on_key(key(KeyCode::Down));
    assert_eq!(app.input, "second task");
    app.on_key(key(KeyCode::Down));
    assert_eq!(app.input, "draft");
    // Editing exits recall (a later Up treats the edit as the new draft).
    app.on_key(key(KeyCode::Up));
    assert_eq!(app.input, "second task");
    app.on_key(key(KeyCode::Char('!')));
    assert_eq!(app.input, "second task!");
    // A later Up restarts at the NEWEST entry, then walks back.
    app.on_key(key(KeyCode::Up));
    assert_eq!(app.input, "second task", "edit restarted history traversal");
    app.on_key(key(KeyCode::Up));
    assert_eq!(app.input, "first task");
    // Resubmitting the NEWEST entry does not duplicate it.
    app.input = "first task".into();
    let _ = app.on_key(key(KeyCode::Enter));
    assert_eq!(app.input_history.len(), 3, "older entries may repeat");
    app.input = "first task".into();
    let _ = app.on_key(key(KeyCode::Enter));
    assert_eq!(app.input_history.len(), 3, "consecutive duplicate deduped");
}

#[test]
fn committed_long_lines_wrap_into_history_rows() {
    let mut app = App {
        last_hist_width: 50,
        ..App::default()
    };
    app.note("x".repeat(120));
    // 120 chars at width 50 -> 3 rows (50 + 50 + 20).
    assert_eq!(app.history.len(), 3, "{:?}", app.history.len());
    let joined: String = app
        .history
        .iter()
        .flat_map(|l| l.iter().flat_map(|s| s.content.chars()))
        .collect();
    assert_eq!(joined.chars().count(), 120);

    // Task echoes wrap too (begin_task path).
    app.history.clear();
    app.begin_task(&"y".repeat(75));
    assert_eq!(app.history.len(), 2, "75 chars + prompt wraps at 50");
}

#[test]
fn model_menu_s_key_saves_as_default_and_config_writer_roundtrips() {
    use crossterm::event::KeyCode;
    // 's' on a highlighted model parks the switch AND the save.
    let mut app = App {
        current_model: "glm-5.3".into(),
        ..App::default()
    };
    app.handle_slash_test(SlashCommand::Model(String::new()));
    app.on_key(key(KeyCode::Down)); // a different row
    app.on_key(key(KeyCode::Char('S'))); // shift+S saves as default
    assert!(app.menu.is_none());
    match &app.pending_command {
        Some(PendingCommand::Model { name, save_default }) => {
            assert!(!name.is_empty());
            assert!(*save_default, "s = switch + save default");
        }
        other => panic!("expected Model save, got {other:?}"),
    }

    // The config writer: MODRON_CONFIG temp dir, existing keys preserved.
    let dir = std::env::temp_dir().join(format!("modron-model-default-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.toml"),
        "router_mode = \"learned\"\ndefault_model = \"old\"\n",
    )
    .unwrap();
    std::env::set_var("MODRON_CONFIG", &dir);
    let path = modron_tui::save_default_model("gpt-5.6-sol", "chatgpt").unwrap();
    std::env::remove_var("MODRON_CONFIG");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("default_model = \"gpt-5.6-sol\""), "{text}");
    assert!(text.contains("default_provider = \"chatgpt\""), "{text}");
    assert!(
        text.contains("router_mode = \"learned\""),
        "other keys kept: {text}"
    );
    assert_eq!(
        text.matches("default_model").count(),
        1,
        "replaced, not duplicated: {text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn zai_browser_sign_in_parks_the_login_flow() {
    use crossterm::event::KeyCode;
    let mut app = App::default();
    app.handle_slash_test(SlashCommand::Auth(String::new()));
    // Registry order: anthropic(0), openai(1), zai(2).
    for _ in 0..2 {
        app.on_key(key(KeyCode::Down));
    }
    app.on_key(key(KeyCode::Enter)); // method choice
    app.on_key(key(KeyCode::Down)); // browser sign-in
    app.on_key(key(KeyCode::Enter));
    assert!(matches!(
        app.auth_flow,
        Some(AuthFlow::WaitingOAuth { info: ref i, ref url, user_code: None })
            if i.provider_id == "zai" && url.starts_with("https://chat.z.ai/api/oauth/authorize")
    ));
    assert!(matches!(
        app.pending_command,
        Some(PendingCommand::ZaiLogin { .. })
    ));
    // Esc backs out to the method screen.
    app.on_key(key(KeyCode::Esc));
    assert!(matches!(
        app.auth_flow,
        Some(AuthFlow::MethodChoice { info: ref i, .. }) if i.provider_id == "zai"
    ));
}

#[test]
fn tool_summary_counts_and_panel_expand_flow() {
    use crossterm::event::KeyCode;
    use modron_core::ToolActivity;
    let mut app = App::default();
    app.begin_task("do things");
    for name in ["glob", "glob", "glob", "read", "read", "grep"] {
        app.on_tool(ToolActivity::Start { name: name.into() });
        app.on_tool(ToolActivity::Done {
            name: name.into(),
            is_error: false,
            bytes: 100,
        });
    }
    let id = modron_core::SessionId::new_v4();
    app.finish_turn("done".into(), Usage::default(), 1, 6, id);
    // Counts, biggest first, capped display.
    let summary = history_text(&app)
        .into_iter()
        .find(|l| l.starts_with("⏺"))
        .expect("summary line");
    assert!(summary.contains("6 tools"), "{summary}");
    assert!(summary.contains("glob ×3"), "{summary}");
    assert!(summary.contains("read ×2"), "{summary}");
    assert!(summary.contains("grep"), "{summary}");

    // ctrl+t opens the panel with the logged events; Esc closes without
    // aborting a running turn.
    assert!(app.tool_panel.is_none());
    app.on_key(crossterm::event::KeyEvent::new(
        KeyCode::Char('t'),
        crossterm::event::KeyModifiers::CONTROL,
    ));
    assert!(app.tool_panel.is_some());
    assert_eq!(app.tool_panel.as_ref().unwrap().len(), 6);
    app.running = true;
    let action = app.on_key(key(KeyCode::Esc));
    assert!(app.tool_panel.is_none(), "esc closes the panel");
    assert!(!matches!(action, crate::Action::Abort), "esc consumed");

    // Clicking the summary line in history re-opens that turn's log.
    let (idx, _) = app.tool_log[0].clone();
    app.hist_geom = (0, idx + 1, true, 0, 0);
    app.begin_selection(0, idx as u16);
    assert!(app.tool_panel.is_some(), "summary click expands");
}

#[test]
fn menus_and_auth_picker_filter_as_you_type() {
    use crossterm::event::KeyCode;
    // /model: typing narrows the catalog; enter picks the first match;
    // backspace widens; esc clears the filter before closing.
    let mut app = App::default();
    app.handle_slash_test(SlashCommand::Model(String::new()));
    for c in "gpt".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    assert!(app.filtered_menu_indices_count() >= 1, "gpt models match");
    let frame = render(&mut app, 100, 24);
    assert!(frame.contains("filter: \"gpt\""), "{frame:?}");
    app.on_key(key(KeyCode::Enter));
    assert!(matches!(
        app.pending_command,
        Some(PendingCommand::Model { ref name, .. }) if name.starts_with("gpt")
    ));
    assert!(app.list_filter.is_empty(), "cleared after selection");

    // Backspace edits the filter; esc clears it first, then closes.
    let mut app = App::default();
    app.handle_slash_test(SlashCommand::Model(String::new()));
    for c in "zzz".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    assert!(app.menu.is_some());
    let frame = render(&mut app, 100, 24);
    assert!(frame.contains("no matches"), "{frame:?}");
    app.on_key(key(KeyCode::Backspace));
    assert_eq!(app.list_filter, "zz");
    app.on_key(key(KeyCode::Esc));
    assert!(app.menu.is_some(), "esc clears the filter first");
    assert!(app.list_filter.is_empty());
    app.on_key(key(KeyCode::Esc));
    assert!(app.menu.is_none(), "second esc closes");

    // /auth picker: filter to the z.ai row and open its method choice.
    let mut app = App::default();
    app.handle_slash_test(SlashCommand::Auth(String::new()));
    for c in "zai".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    app.on_key(key(KeyCode::Enter));
    match &app.auth_flow {
        Some(AuthFlow::MethodChoice { info, .. }) => {
            assert_eq!(info.provider_id, "zai")
        }
        other => panic!("filter should land on z.ai, got {other:?}"),
    }
}

#[test]
fn model_menu_marks_unauthenticated_providers() {
    let dir = std::env::temp_dir().join(format!("modron-nokey-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("MODRON_CONFIG", &dir);
    let mut app = App::default();
    app.handle_slash_test(SlashCommand::Model(String::new()));
    std::env::remove_var("MODRON_CONFIG");
    // With no keys or sign-ins in the isolated config, every provider
    // row carries the no-key marker; filtering still works over it.
    assert!(app.filtered_menu_indices_count() >= 12);
    for c in "no key".chars() {
        app.on_key(key(crossterm::event::KeyCode::Char(c)));
    }
    // "no key" appears in DETAIL, which the filter matches.
    assert!(
        app.filtered_menu_indices_count() >= 1,
        "detail participates in filtering"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ── Status strip, toasts, `!` passthrough, `@file` completion ─────────────

#[test]
fn status_strip_consolidates_the_dense_line() {
    let mut app = App {
        model_label: "glm-5.3".into(),
        context_window: 200_000,
        context_used: 60_000,
        approval_mode: "ask".into(),
        ..App::default()
    };
    app.cumulative.input_tokens = 300;
    app.cumulative.cached_input_tokens = 100;
    let line = app.status_line();
    assert!(line.contains("glm-5.3"), "model first: {line}");
    assert!(line.contains("300 tok"), "cumulative tokens: {line}");
    assert!(line.contains("cache 25%"), "session cache hit: {line}");
    assert!(line.contains("ctx 30%"), "estimated context: {line}");
    assert!(line.contains("ask (shift+tab)"), "mode + key hint: {line}");
    assert!(line.contains("? shortcuts"), "help hint: {line}");
    assert!(line.contains("no session"), "session state kept: {line}");

    // Running: turn number, in-flight tool and interrupt hint survive.
    app.running = true;
    app.turn_count = 3;
    app.current_tool = Some("grep".into());
    let line = app.status_line();
    assert!(line.contains("turn 4"), "{line}");
    assert!(line.contains("grep"), "{line}");
    assert!(line.contains("esc interrupt"), "{line}");

    // Context pressure flips the ctx segment to the compact warning.
    app.context_used = 180_000;
    assert!(
        app.status_line().contains("ctx 90%!"),
        "compaction warning: {}",
        app.status_line()
    );
}

#[test]
fn bare_question_mark_shows_shortcuts() {
    use modron_tui::HelpTab;
    let mut app = App::default();
    app.on_key(key(crossterm::event::KeyCode::Char('?')));
    assert_eq!(
        app.on_key(key(crossterm::event::KeyCode::Enter)),
        Action::None,
        "help is handled locally"
    );
    assert!(
        matches!(app.help, Some(HelpTab::General)),
        "bare ? opens the same tabbed modal as /help"
    );
}

// ── Tabbed help modal (/help) ──────────────────────────────────────────────

#[test]
fn help_modal_tabs_switch_scroll_and_esc_closes() {
    use crossterm::event::KeyCode;
    use modron_tui::HelpTab;
    let mut app = App::default();
    app.handle_slash_test(SlashCommand::Help);
    assert!(matches!(app.help, Some(HelpTab::General)));

    // Right (and vim h/l) walk the ring, wrapping at both ends.
    app.on_key(key(KeyCode::Right));
    assert!(matches!(app.help, Some(HelpTab::Commands)));
    app.on_key(key(KeyCode::Right));
    assert!(matches!(app.help, Some(HelpTab::Shortcuts)));
    app.on_key(key(KeyCode::Right));
    assert!(matches!(app.help, Some(HelpTab::General)), "wraps forward");
    app.on_key(key(KeyCode::Char('h')));
    assert!(matches!(app.help, Some(HelpTab::Shortcuts)), "h walks back");
    app.on_key(key(KeyCode::Left));
    assert!(
        matches!(app.help, Some(HelpTab::Commands)),
        "left walks back"
    );

    // The modal owns the keyboard: typing never leaks into the input
    // underneath, and one esc closes the modal (the input survives it).
    app.input.push_str("leftover");
    app.on_key(key(KeyCode::Char('x')));
    assert_eq!(app.input, "leftover", "typing is swallowed");
    app.on_key(key(KeyCode::Esc));
    assert!(app.help.is_none());
    assert_eq!(app.input, "leftover", "esc only closes the modal");

    // Esc with a turn running closes the modal instead of aborting it
    // (the modal check sits before the Esc-abort branch).
    let mut run = App {
        running: true,
        ..App::default()
    };
    run.handle_slash_test(SlashCommand::Help);
    assert_eq!(run.on_key(key(KeyCode::Esc)), Action::None);
    assert!(run.help.is_none());

    // The Commands tab is taller than the window: downs clamp at the
    // content's end on render, up walks back.
    app.handle_slash_test(SlashCommand::Help);
    app.on_key(key(KeyCode::Right)); // Commands
    for _ in 0..50 {
        app.on_key(key(KeyCode::Down));
    }
    let _ = render(&mut app, 90, 24); // render performs the clamp
    assert!(app.help_scroll < 50, "scroll clamped: {}", app.help_scroll);
    app.on_key(key(KeyCode::Up));
    assert_eq!(app.help_scroll, 22, "one down-step back from the clamp");
}

#[test]
fn help_modal_renders_tabs_catalog_and_shortcuts() {
    use crossterm::event::KeyCode;
    let mut app = App::default();
    app.handle_slash_test(SlashCommand::Help);
    let frame = render(&mut app, 90, 24);
    assert!(frame.contains("General"), "tab strip: {frame:?}");
    assert!(frame.contains("Commands"), "every tab labelled: {frame:?}");
    assert!(
        frame.contains("token-efficient coding agent"),
        "general copy: {frame:?}"
    );
    assert!(frame.contains("←/→ tabs"), "footer hint: {frame:?}");

    // Commands tab renders straight from the catalog (so it lists
    // exactly what parses).
    app.on_key(key(KeyCode::Right));
    let frame = render(&mut app, 90, 24);
    assert!(frame.contains("/compact"), "catalog rows: {frame:?}");
    assert!(frame.contains("/help"), "catalog rows: {frame:?}");

    // Shortcuts tab: only bindings that actually exist.
    app.on_key(key(KeyCode::Right));
    let frame = render(&mut app, 90, 24);
    assert!(frame.contains("pgup/pgdn"), "shortcut grid: {frame:?}");
    assert!(frame.contains("shift+tab"), "shortcut grid: {frame:?}");
    assert!(frame.contains("ctrl+v"), "shortcut grid: {frame:?}");
}

#[test]
fn shift_tab_flips_approval_mode_and_parks_the_engine_mutation() {
    use crossterm::event::KeyCode;
    let mut app = App {
        approval_mode: "ask".into(),
        ..App::default()
    };
    assert_eq!(app.on_key(key(KeyCode::BackTab)), Action::None);
    assert_eq!(app.approval_mode, "yolo");
    assert_eq!(
        app.pending_command,
        Some(PendingCommand::Mode("yolo".into()))
    );
    assert_eq!(app.on_key(key(KeyCode::BackTab)), Action::None);
    assert_eq!(app.approval_mode, "ask");
}

#[test]
fn toast_note_expires_after_forty_ticks() {
    let mut app = App::default();
    app.toast("copying 12 chars…");
    assert_eq!(app.status_note.as_deref(), Some("copying 12 chars…"));
    for _ in 0..39 {
        app.tick();
    }
    assert!(app.status_note.is_some(), "still visible at tick 39");
    app.tick();
    assert!(app.status_note.is_none(), "auto-cleared at tick 40");
}

#[test]
fn toast_renders_above_the_input_then_expires() {
    let mut app = App {
        model_label: "glm-5.3".into(),
        ..App::default()
    };
    app.toast("attached [img shot.png]");
    let frame = render(&mut app, 80, 8);
    assert!(frame.contains("attached [img shot.png]"), "{frame:?}");
    for _ in 0..40 {
        app.tick();
    }
    let frame = render(&mut app, 80, 8);
    assert!(!frame.contains("attached"), "toast aged out: {frame:?}");
}

#[test]
fn shell_passthrough_detection_and_output_cap() {
    use modron_tui::run::{cap_output, shell_passthrough};
    assert_eq!(shell_passthrough("fix the bug"), None, "plain task");
    assert_eq!(
        shell_passthrough("!git status"),
        Some("git status".into()),
        "the ! prefix is stripped"
    );
    assert_eq!(
        shell_passthrough("!   cargo fmt  "),
        Some("cargo fmt".into()),
        "surrounding whitespace trimmed"
    );
    assert_eq!(
        shell_passthrough("!"),
        Some(String::new()),
        "bare ! = usage"
    );

    let long = "a".repeat(20_000);
    let capped = cap_output(&long);
    assert!(capped.len() < 11_000, "bounded: {}", capped.len());
    assert!(capped.contains("truncated"), "the cut is marked");

    // Multi-byte safety: the tail stays whole chars (a String, so a split
    // char would have panicked the boundary walk).
    let wide = "🦀".repeat(5_000);
    let capped = cap_output(&wide);
    assert!(capped.len() < 10_500, "bounded: {}", capped.len());
    assert!(capped.ends_with("🦀"), "tail kept whole: {capped:?}");
    assert_eq!(cap_output("short"), "short", "under the cap, verbatim");
}

#[test]
fn at_token_is_the_last_boundary_at_with_fragment() {
    let mut app = App::default();
    assert!(app.at_token().is_none(), "no @, no token");

    app.input = "fix @src/li".into();
    let (at, frag) = app.at_token().unwrap();
    assert_eq!(at, 4);
    assert_eq!(frag, "src/li");

    // An email address is not a completion token (no boundary before @).
    app.input = "mail jacob@example.com".into();
    assert!(app.at_token().is_none());

    // Whitespace after the fragment ends the token (bare-@ semantics for
    // the token itself; the fragment stops at the space).
    app.input = "@src main.rs".into();
    let (_, frag) = app.at_token().unwrap();
    assert_eq!(frag, "src");
}

#[test]
fn at_completion_walks_the_workspace_and_tab_completes() {
    use crossterm::event::KeyCode;
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), "fn main() {}").unwrap();
    std::fs::write(dir.path().join("README.md"), "hi").unwrap();
    std::fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
    std::fs::write(dir.path().join("ignored.txt"), "").unwrap();
    std::fs::create_dir_all(dir.path().join("node_modules")).unwrap();
    std::fs::write(dir.path().join("node_modules/x.js"), "").unwrap();
    let mut app = App {
        working_dir: dir.path().to_path_buf(),
        ..App::default()
    };

    // Case-insensitive substring filter over the cached walk.
    app.input = "read @read".into();
    assert_eq!(app.file_matches(), vec!["README.md"]);
    app.cursor = app.input.chars().count();
    assert_eq!(app.on_key(key(KeyCode::Tab)), Action::None);
    assert_eq!(app.input, "read @README.md ", "token spliced in place");
    assert_eq!(app.cursor, "read @README.md ".chars().count());

    // Submit invalidates the walk cache (the task may change the tree).
    app.on_key(key(KeyCode::Enter));
    assert!(
        app.file_completions.is_none(),
        "cache invalidated on submit"
    );

    // The next @ re-walks: gitignored entries and vendored trees never
    // surface; directories complete too.
    app.input = "@".into();
    let all = app.file_matches();
    assert!(all.contains(&"src".to_string()), "{all:?}");
    assert!(all.contains(&"src/lib.rs".to_string()), "{all:?}");
    assert!(
        !all.iter()
            .any(|f| f.ends_with("ignored.txt") || f.contains("node_modules")),
        "gitignore + vendor filters: {all:?}"
    );
}

#[test]
fn at_popup_renders_in_the_popup_slot() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("README.md"), "hi").unwrap();
    let mut app = App {
        working_dir: dir.path().to_path_buf(),
        model_label: "glm-5.3".into(),
        ..App::default()
    };
    app.input = "@".into();
    let frame = render(&mut app, 80, 12);
    assert!(frame.contains("@README.md"), "completion popup: {frame:?}");
    assert!(frame.contains("glm-5.3"), "status strip still present");
}
