//! The TUI run loop: inline viewport + crossterm events + engine task.
//!
//! Threading model (see LEDGER Phase 1d): the render loop owns the terminal
//! and polls at ~20fps (no 60fps chase — Claude Code's alt-screen mode exists
//! because of redraw cost, and we avoid that cost by only rendering the live
//! region). The engine runs on the Tokio runtime; deltas/tool events arrive
//! over std channels; approvals queue into a shared slot and park the engine
//! task via `tokio::task::block_in_place` until the user decides.

use crate::{App, PendingApproval};
use modron_core::{
    Approver, DeltaSink, Engine, RunSummary, SessionId, StreamDelta, ToolActivity,
    ToolActivitySink, ToolCall,
};
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

/// What the engine task reports when a turn settles.
pub enum Outcome {
    Done(RunSummary),
    Failed(String),
    Aborted,
}

/// Sinks the TUI hands to the engine builder (wired to its channels).
#[derive(Clone)]
pub struct TuiSinks {
    pub on_delta: Arc<DeltaSink>,
    pub on_tool_event: Arc<ToolActivitySink>,
    pub approver: Arc<Approver>,
}

#[derive(Clone)]
pub struct TuiConfig {
    pub resume: Option<SessionId>,
    pub model_label: String,
}

/// Minimum live-region height (input box 3 + status 1 + breathing room).
const VIEWPORT_MIN: u16 = 5;
/// Cap for the live region — the terminal's native scrollback owns the rest.
const VIEWPORT_MAX: u16 = 24;
const TICK: Duration = Duration::from_millis(50);

/// Shared approval slot: the engine-side approver queues a call; the render
/// loop drains it into the App; the response channel carries the decision.
type ApprovalSlot = Arc<Mutex<Option<(ToolCallLike, mpsc::Sender<bool>)>>>;

#[derive(Clone)]
struct ApprovalBus {
    slot: ApprovalSlot,
    rest_approved: Arc<std::sync::atomic::AtomicBool>,
}

// The slot carries the tool-call summary text, not the ToolCall itself (the
// approver receives a &ToolCall and we only need its display form).
type ToolCallLike = String;

/// Run the TUI. `build` receives the wired sinks and returns the engine plus
/// the session to resume (if `--resume` was given). Requires a TTY on stdout;
/// returns an actionable error otherwise.
pub async fn run_with(
    build: impl FnOnce(TuiSinks) -> (Engine, Option<SessionId>),
    model_label: String,
    upgrade_notice: Option<String>,
) -> anyhow::Result<()> {
    use std::io::IsTerminal as _;
    if !std::io::stdout().is_terminal() {
        anyhow::bail!(
            "the TUI needs a terminal on stdout — use `modron chat` (piped stdin) \
             or `modron \"task\"` (one-shot) here"
        );
    }

    let (delta_tx, delta_rx) = mpsc::channel::<DeltaOrTool>();
    let approval = ApprovalBus {
        slot: Arc::new(Mutex::new(None)),
        rest_approved: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };

    // The engine builder gets sinks that talk to THIS loop.
    let delta_sink_tx = delta_tx.clone();
    let tool_tx = delta_tx.clone();
    let approval_for_builder = approval.clone();
    let (engine, resume_session) = build(TuiSinks {
        on_delta: Arc::new(move |delta| {
            let _ = delta_sink_tx.send(DeltaOrTool::Delta(delta));
        }),
        on_tool_event: Arc::new(move |activity| {
            let _ = tool_tx.send(DeltaOrTool::Tool(activity));
        }),
        approver: Arc::new(move |call| {
            if approval_for_builder.rest_approved.load(Ordering::Relaxed) {
                return true;
            }
            let (respond, receiver) = mpsc::channel();
            *approval_for_builder.slot.lock().unwrap() = Some((short_call(call), respond));
            // Park the engine task until the render loop delivers the
            // decision. block_in_place keeps other runtime workers alive.
            tokio::task::block_in_place(|| receiver.recv().unwrap_or(false))
        }),
    });

    // Terminal setup: inline viewport, raw mode, hidden cursor.
    let mut stdout = std::io::stdout();
    crossterm::terminal::enable_raw_mode()?;
    // NO alternate screen: the normal buffer keeps native scrollback and
    // selectable text (Claude Code's inline pattern). The earlier
    // EnterAlternateScreen here was the black-void bug — the alt screen is
    // an empty buffer with no history.
    //
    // Bottom anchoring: push the cursor to the terminal's last row before
    // creating the inline viewport, so the input box pins to the bottom
    // (like a chat). The padding lines scroll away as content commits.
    {
        use std::io::Write as _;
        let rows = crossterm::terminal::size().map(|(_, r)| r).unwrap_or(24);
        for _ in 0..rows.saturating_sub(1) {
            let _ = writeln!(stdout);
        }
        let _ = stdout.flush();
    }
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::with_options(
        backend,
        ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Inline(VIEWPORT_MIN),
        },
    )?;

    // Welcome banner (Phase 6g): the TUI opens onto content, not a void.
    {
        use ratatui::style::{Modifier, Style};
        use ratatui::text::{Line, Span};
        let welcome: Vec<Line<'static>> = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled("✦ ", Style::default().fg(crate::ACCENT)),
                Span::styled(
                    "modron",
                    Style::default()
                        .fg(crate::ACCENT)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" v{}", env!("CARGO_PKG_VERSION")),
                    Style::default().fg(crate::DIM),
                ),
            ]),
            Line::from(Span::styled(
                "  a token-efficient Rust coding agent",
                Style::default().fg(crate::DIM),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled("  model ", Style::default().fg(crate::DIM)),
                Span::styled(model_label.clone(), Style::default()),
                Span::styled(
                    "  ·  enter send · esc interrupt · ctrl+o detail",
                    Style::default().fg(crate::DIM),
                ),
            ]),
            Line::from(""),
        ];
        for line in &welcome {
            let _ = commit(&mut terminal, line);
        }
    }

    let mut app = App {
        upgrade_notice,

        session_id: resume_session,
        ..App::default()
    };
    let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel::<Outcome>();
    let mut engine_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut last_viewport_height: u16 = VIEWPORT_MIN;
    #[allow(unused_assignments)]
    let mut result: anyhow::Result<()> = Ok(());

    loop {
        // 1. Drain engine deltas/tool events.
        while let Ok(msg) = delta_rx.try_recv() {
            match msg {
                DeltaOrTool::Delta(d) => app.on_delta(d),
                DeltaOrTool::Tool(t) => app.on_tool(t),
            }
        }

        // 2. Drain the approval slot into the App (redraw happens below).
        if app.pending_approval.is_none() {
            if let Some((summary, respond)) = approval.slot.lock().unwrap().take() {
                app.pending_approval = Some(PendingApproval {
                    call: modron_core::ToolCall {
                        id: String::new(),
                        name: summary
                            .split_once(' ')
                            .map(|(n, _)| n.to_string())
                            .unwrap_or(summary.clone()),
                        arguments: serde_json::Value::String(
                            summary
                                .split_once(' ')
                                .map(|(_, a)| a.to_string())
                                .unwrap_or_default(),
                        ),
                    },
                    respond,
                });
                // The display summary rides in `arguments` as a string; the
                // status line prints name + args, so this reconstructs it.
            }
        }

        // 3. Poll keyboard (raw mode).
        if crossterm::event::poll(TICK)? {
            if let crossterm::event::Event::Key(key) = crossterm::event::read()? {
                if key.kind == crossterm::event::KeyEventKind::Press {
                    match app.on_key(key) {
                        crate::Action::None => {}
                        crate::Action::Submit(task) => {
                            app.begin_task(&task);
                            let engine = engine.clone();
                            let done = done_tx.clone();
                            let session = app.session_id;
                            engine_task = Some(tokio::spawn(async move {
                                let outcome = match session {
                                    Some(id) => engine.run_task_resuming(id, &task).await,
                                    None => engine.run_task(&task).await,
                                };
                                match outcome {
                                    Ok(summary) => {
                                        let _ = done.send(Outcome::Done(summary));
                                    }
                                    Err(e) => {
                                        let _ = done.send(Outcome::Failed(e.to_string()));
                                    }
                                }
                            }));
                        }
                        crate::Action::Abort => {
                            if let Some(task) = engine_task.take() {
                                task.abort();
                                // Give the runtime a beat to cancel.
                                tokio::time::sleep(Duration::from_millis(30)).await;
                            }
                            for line in app.finish_interrupted() {
                                commit(&mut terminal, &line)?;
                            }
                        }
                        crate::Action::Quit => {
                            if let Some(task) = engine_task.take() {
                                task.abort();
                            }
                            result = Ok(());
                            break;
                        }
                    }
                }
            }
        }

        // 4. Approve-rest mirror: when the user pressed `a`, future approvals
        //    short-circuit in the approver; drain any queued one just in case.
        if app.approve_rest {
            approval
                .rest_approved
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }

        // 5. Engine settled?
        while let Ok(outcome) = done_rx.try_recv() {
            engine_task = None;
            match outcome {
                Outcome::Done(summary) => {
                    app.clear_subagents();
                    let lines = app.finish_turn(
                        summary.answer.clone(),
                        summary.usage,
                        summary.turns,
                        summary.tool_calls,
                        summary.session_id,
                    );
                    for line in &lines {
                        commit(&mut terminal, line)?;
                    }
                }
                Outcome::Failed(e) => {
                    for line in app.finish_interrupted() {
                        commit(&mut terminal, &line)?;
                    }
                    commit(
                        &mut terminal,
                        &ratatui::text::Line::from(format!("error: {e}"))
                            .style(ratatui::style::Style::default().fg(ratatui::style::Color::Red)),
                    )?;
                }
                Outcome::Aborted => {}
            }
        }

        if app.quit {
            result = Ok(());
            break;
        }

        // 6. Render the live region; advance the spinner.
        // Reactive viewport (Phase 6g): grows with content, shrinks idle.
        let term_size = terminal.size()?;
        let streaming_h = if app.streaming.is_empty() {
            0
        } else {
            (term_size.height / 3).clamp(3, 12)
        };
        let desired = 3
            + 1
            + app.subagents.len() as u16
            + streaming_h
            + u16::from(app.pending_approval.is_some());
        let new_height = desired.clamp(VIEWPORT_MIN, term_size.height.min(VIEWPORT_MAX));
        if new_height != last_viewport_height {
            let _ = terminal.resize(ratatui::layout::Rect::new(
                0,
                0,
                term_size.width,
                new_height,
            ));
            last_viewport_height = new_height;
        }

        app.spinner_tick += 1;
        terminal.draw(|frame| app.view(frame, frame.area()))?;
    }

    // Teardown: flush the last status as a plain line so scrollback keeps a
    // trace of the session, restore the terminal.
    crossterm::terminal::disable_raw_mode()?;
    if let Some(id) = app.session_id {
        println!("session: {id} — resume with --resume {id}");
    }
    result
}

/// Commit one line into native scrollback above the live viewport.
fn commit(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    line: &ratatui::text::Line<'static>,
) -> anyhow::Result<()> {
    use ratatui::widgets::Widget as _;
    terminal.insert_before(1, |buf| {
        ratatui::widgets::Paragraph::new(line.clone()).render(buf.area, buf);
    })?;
    Ok(())
}

enum DeltaOrTool {
    Delta(StreamDelta),
    Tool(ToolActivity),
}

fn short_call(call: &ToolCall) -> ToolCallLike {
    let args = call.arguments.to_string();
    let args = if args.chars().count() > 80 {
        format!("{}…", args.chars().take(80).collect::<String>())
    } else {
        args
    };
    format!("{} {}", call.name, args)
}
