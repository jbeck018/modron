//! Phase 1d TUI tests: the App state machine (pure) and the live-region
//! rendering via ratatui's TestBackend. The terminal loop itself needs a
//! real TTY and is intentionally thin.

use modron_core::{StreamDelta, ToolActivity, ToolCall, Usage};
use modron_tui::{Action, App, PendingApproval, PendingCommand, SlashCommand};
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
    // /help shows help text in the tool lines
    app.on_key(key(crossterm::event::KeyCode::Char('/')));
    for c in "help".chars() {
        app.on_key(key(crossterm::event::KeyCode::Char(c)));
    }
    let action = app.on_key(key(crossterm::event::KeyCode::Enter));
    assert_eq!(action, Action::None, "help is local, not submitted");
    assert!(app.tool_lines.iter().any(|l| l.contains("/exit")));

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
    assert_eq!(app.tool_lines, vec!["❯ one", "  two", "  three"]);
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
    assert!(app
        .tool_lines
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
fn theme_slash_lists_and_switches() {
    let mut app = App::default();
    assert_eq!(app.theme.name, "modron");
    app.handle_slash_test(SlashCommand::Theme(String::new()));
    assert!(
        app.tool_lines
            .iter()
            .any(|l| l.contains("midnight") && l.contains("mono")),
        "{:?}",
        app.tool_lines
    );
    app.handle_slash_test(SlashCommand::Theme("midnight".into()));
    assert_eq!(app.theme.name, "midnight");
    assert_eq!(
        app.theme.resolved().accent,
        ratatui::style::Color::Rgb(122, 162, 247)
    );
    app.handle_slash_test(SlashCommand::Theme("no-such-theme".into()));
    assert_eq!(app.theme.name, "midnight", "unknown theme does not switch");
    assert!(
        app.tool_lines.iter().any(|l| l.contains("unknown theme")),
        "{:?}",
        app.tool_lines
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
