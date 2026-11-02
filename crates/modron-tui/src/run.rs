//! The TUI run loop: inline viewport + crossterm events + engine task.
//!
//! Threading model (see LEDGER Phase 1d): the render loop owns the terminal
//! and polls at ~20fps (no 60fps chase — Claude Code's alt-screen mode exists
//! because of redraw cost, and we avoid that cost by only rendering the live
//! region). The engine runs on the Tokio runtime; deltas/tool events arrive
//! over std channels; approvals queue into a shared slot and park the engine
//! task via `tokio::task::block_in_place` until the user decides.

use crate::{App, PendingApproval, PendingCommand};
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

/// Fixed live-region height: input box + status + streaming tail. The
/// earlier reactive grow/shrink viewport repositioned ratatui's inline
/// region mid-session and left stale frames frozen in scrollback (the
/// ghost "working" input box). A fixed region with internal flex layout
/// (streaming/subagents/approval trade space inside it) is deterministic.
const VIEWPORT_HEIGHT: u16 = 10;
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
    let (mut engine, resume_session) = build(TuiSinks {
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
    // Kitty keyboard protocol (kitty/Ghostty/WezTerm/iTerm2 3.5+): lets the
    // terminal report modifiers on Enter, making shift+enter a real newline
    // instead of a submit. The query returns quickly on any terminal that
    // answers device attributes; unsupported ones ignore the push entirely.
    let kitty_keyboard = crossterm::terminal::supports_keyboard_enhancement()
        .unwrap_or(false);
    let mut stdout = std::io::stdout();
    crossterm::terminal::enable_raw_mode()?;
    // Bracketed paste (ctrl+V in most terminals delivers a paste event):
    // multi-line pastes enter the input instead of submitting.
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::EnableBracketedPaste
    );
    if kitty_keyboard {
        use crossterm::event::{
            KeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
        };
        let _ = crossterm::execute!(
            stdout,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            )
        );
    }
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
            viewport: ratatui::Viewport::Inline(
                // Fit small terminals, but never resize after this.
                crossterm::terminal::size()
                    .map(|(_, r)| r)
                    .unwrap_or(24)
                    .saturating_sub(1)
                    .min(VIEWPORT_HEIGHT)
                    .max(3),
            ),
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
                    "  ·  enter send · shift/alt+enter newline · \\+enter too · esc interrupt · ctrl+o detail",
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
        model_label: model_label.clone(),
        context_window: modron_router::lookup(&model_label)
            .map(|info| info.context_window)
            .unwrap_or(0),
        ..App::default()
    };
    let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel::<Outcome>();
    let mut engine_task: Option<tokio::task::JoinHandle<()>> = None;
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
            match crossterm::event::read()? {
                crossterm::event::Event::Paste(text) => {
                    // Bracketed paste: multi-line text enters the input
                    // verbatim — the terminal told us it is a paste, so it
                    // must never submit.
                    let text = text.replace("\r\n", "\n").replace('\r', "\n");
                    app.input.push_str(&text);
                    continue;
                }
                crossterm::event::Event::Key(key) => {
                if key.kind == crossterm::event::KeyEventKind::Press {
                    match app.on_key(key) {
                        crate::Action::None => {}
                        crate::Action::PasteImage => {
                            // ctrl+V: image → staged attachment; text → into
                            // the input. Bounded so a hung clipboard service
                            // cannot hang the TUI.
                            let started = std::time::Instant::now();
                            match tokio::task::spawn_blocking(crate::clipboard::harvest)
                                .await
                                .unwrap_or(crate::clipboard::ClipboardContent::Empty)
                            {
                                crate::clipboard::ClipboardContent::Image(path) => {
                                    match app.attach_image(&path) {
                                        Some(chip) => app.status_note = Some(format!(
                                            "attached {chip} (goes with your next message)"
                                        )),
                                        None => app.status_note =
                                            Some("clipboard image unreadable".into()),
                                    }
                                }
                                crate::clipboard::ClipboardContent::Text(text) => {
                                    let text = text.replace("\r\n", "\n").replace('\r', "\n");
                                    app.input.push_str(&text);
                                }
                                crate::clipboard::ClipboardContent::Empty => {
                                    app.status_note =
                                        Some("clipboard empty".into());
                                }
                            }
                            let _ = started; // bounded by the harvest timeout itself
                        }
                        crate::Action::Submit(task) => {
                            let task = app.task_with_goal(&task);
                            app.begin_task(&task);
                            let attachments = app.take_attachments();
                            let engine = engine.clone();
                            let done = done_tx.clone();
                            let session = app.session_id;
                            engine_task = Some(tokio::spawn(async move {
                                let outcome = match session {
                                    Some(id) => {
                                        engine
                                            .run_task_resuming_with_attachments(
                                                id, &task, attachments,
                                            )
                                            .await
                                    }
                                    None => {
                                        engine
                                            .run_task_with_attachments(&task, attachments)
                                            .await
                                    }
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
                            blank_frame(&mut terminal)?;
                            let lines = app.finish_interrupted();
                            commit_block(&mut terminal, &lines)?;
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
                _ => {}
            }
        }

        // 3b. Slash commands needing engine/store powers (Phase 14): the
        //     App parks a PendingCommand; execute it here, then commit the
        //     result lines immediately (not at the next turn's end).
        if let Some(cmd) = app.pending_command.take() {
            let mut lines = run_pending_command(&mut engine, &mut app, cmd).await;
            lines.extend(std::mem::take(&mut app.tool_lines));
            if !lines.is_empty() {
                blank_frame(&mut terminal)?;
                let owned: Vec<ratatui::text::Line<'static>> = lines
                    .into_iter()
                    .map(|l| {
                        ratatui::text::Line::from(l)
                            .style(ratatui::style::Style::default().fg(crate::DIM))
                    })
                    .collect();
                commit_block(&mut terminal, &owned)?;
            }
        } else if !app.running && !app.tool_lines.is_empty() {
            // Purely local slash output (/help, /model, …): show it now
            // instead of waiting for the next completed turn.
            let lines: Vec<ratatui::text::Line<'static>> = std::mem::take(&mut app.tool_lines)
                .into_iter()
                .map(|l| {
                    ratatui::text::Line::from(l)
                        .style(ratatui::style::Style::default().fg(crate::DIM))
                })
                .collect();
            blank_frame(&mut terminal)?;
            commit_block(&mut terminal, &lines)?;
        }

        // 4. Approve-rest mirror: when the user pressed `a`, future approvals
        //    short-circuit in the approver; drain any queued one just in case.
        if app.approve_rest {
            approval
                .rest_approved
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }

        // 5. Engine settled? (Re-drain the event channels first: deltas and
        // tool activities can queue behind the Done notification.)
        while let Ok(outcome) = done_rx.try_recv() {
            engine_task = None;
            while let Ok(msg) = delta_rx.try_recv() {
                match msg {
                    DeltaOrTool::Delta(d) => app.on_delta(d),
                    DeltaOrTool::Tool(t) => app.on_tool(t),
                }
            }
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
                    blank_frame(&mut terminal)?;
                    commit_block(&mut terminal, &lines)?;
                }
                Outcome::Failed(e) => {
                    let mut lines = app.finish_interrupted();
                    lines.push(
                        ratatui::text::Line::from(format!("error: {e}"))
                            .style(ratatui::style::Style::default().fg(ratatui::style::Color::Red)),
                    );
                    blank_frame(&mut terminal)?;
                    commit_block(&mut terminal, &lines)?;
                }
                Outcome::Aborted => {}
            }
        }

        if app.quit {
            result = Ok(());
            break;
        }

        // 6. Render the live region; advance the spinner. The region height
        // is FIXED (see VIEWPORT_HEIGHT) — no mid-session resizes.
        app.spinner_tick += 1;
        terminal.draw(|frame| app.view(frame, frame.area()))?;
    }

    // Teardown: flush the last status as a plain line so scrollback keeps a
    // trace of the session, restore the terminal.
    if kitty_keyboard {
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::event::PopKeyboardEnhancementFlags
        );
    }
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::DisableBracketedPaste
    );
    crossterm::terminal::disable_raw_mode()?;
    if let Some(id) = app.session_id {
        println!("session: {id} — resume with --resume {id}");
    }
    result
}

/// Execute a parked slash command against the engine/store. Returns the
/// lines to commit. Engine mutations apply to every FUTURE turn (each
/// submit clones the engine).
async fn run_pending_command(
    engine: &mut Engine,
    app: &mut App,
    cmd: PendingCommand,
) -> Vec<String> {
    match cmd {
        PendingCommand::Agent(name) => {
            let name = name.trim().to_string();
            if name.is_empty() {
                let personas: Vec<String> = engine.config.personas.keys().cloned().collect();
                if personas.is_empty() {
                    vec![
                        "no personas configured — define [agents.<name>] in config.toml".into(),
                    ]
                } else {
                    vec![format!("personas: {} (switch: /agent <name>)", personas.join(", "))]
                }
            } else if let Some(persona) = engine.config.personas.get(&name).cloned() {
                if let Some(system) = persona.system.clone() {
                    engine.config.system = system;
                }
                if let Some(model) = persona.model.clone() {
                    engine.config.default_model = model;
                }
                if let Some(provider) = persona.provider.clone() {
                    engine.config.default_provider = provider;
                }
                engine.config.approvals.allow.extend(persona.allow.clone());
                engine.config.approvals.deny.extend(persona.deny.clone());
                vec![format!("switched to persona `{name}` (model/provider follow if set)")]
            } else {
                vec![format!(
                    "unknown persona `{name}` — configured: {}",
                    engine.config.personas.keys().cloned().collect::<Vec<_>>().join(", ")
                )]
            }
        }
        PendingCommand::Allow(tool) => {
            let tool = tool.trim().to_string();
            if tool.is_empty() {
                return vec![format!(
                    "allow list: {} (add: /allow <tool>)",
                    engine.config.approvals.allow.join(", ")
                )];
            }
            engine.config.approvals.allow.push(tool.clone());
            vec![format!("`{tool}` allowed for the rest of the session")]
        }
        PendingCommand::Deny(tool) => {
            let tool = tool.trim().to_string();
            if tool.is_empty() {
                return vec![format!(
                    "deny list: {} (add: /deny <tool>)",
                    engine.config.approvals.deny.join(", ")
                )];
            }
            engine.config.approvals.deny.push(tool.clone());
            vec![format!("`{tool}` denied for the rest of the session")]
        }
        PendingCommand::Mode(mode) => match mode.trim() {
            "ask" => {
                engine.config.approvals.mode = modron_core::ApprovalMode::Ask;
                vec!["approval mode: ask (mutating tools prompt)".into()]
            }
            "yolo" | "auto" => {
                engine.config.approvals.mode = modron_core::ApprovalMode::ApproveAll;
                vec!["approval mode: yolo (mutating tools auto-approved)".into()]
            }
            other => vec![format!("usage: /mode ask|yolo (got {other:?})")],
        },
        PendingCommand::Tasks => {
            // Run the delegate_status tool directly through the registry.
            let ctx = modron_core::ToolCtx {
                sandbox: engine.config.sandbox,
                working_dir: engine.config.working_dir.clone(),
                engine: Some(std::sync::Arc::new(engine.clone())),
            };
            match engine.tools.get("delegate_status") {
                Some(tool) => match tool.execute(serde_json::json!({}), &ctx).await {
                    Ok(outcome) => vec![outcome.content],
                    Err(e) => vec![format!("delegate_status failed: {e}")],
                },
                None => vec!["delegate_status tool not registered in this build".into()],
            }
        }
        PendingCommand::Worktree => {
            let dir = engine.config.working_dir.join(".modron").join("worktrees");
            match std::fs::read_dir(&dir) {
                Ok(entries) => {
                    let names: Vec<String> = entries
                        .flatten()
                        .map(|e| e.file_name().to_string_lossy().to_string())
                        .collect();
                    if names.is_empty() {
                        vec![format!("no worktrees under {}", dir.display())]
                    } else {
                        vec![format!("worktrees: {}", names.join(", "))]
                    }
                }
                Err(_) => vec![format!("no worktrees yet (created by delegate isolate:true)")],
            }
        }
        PendingCommand::Issues(arg) => run_issues(engine, &arg),
        PendingCommand::Fork => {
            match app.session_id {
                Some(id) => match engine.store.fork(id).await {
                    Ok(new) => {
                        app.session_id = Some(new);
                        vec![format!(
                            "forked: new branch {new} (active now; the original is untouched — \
                             resume it with --resume {id})"
                        )]
                    }
                    Err(e) => vec![format!("fork failed: {e}")],
                },
                None => vec!["no session yet — send a task first, then /fork".into()],
            }
        }
    }
}

/// /issues — a tiny per-project board in the workspace memory store:
/// `<memory_root>/<slug>/issues.jsonl` with `open` state.
fn run_issues(engine: &Engine, arg: &str) -> Vec<String> {
    let Some(root) = engine.config.memory_root.clone() else {
        return vec!["memory not configured — issues live with memory".into()];
    };
    let store = match modron_core::memory::MemoryStore::for_workspace(
        &root,
        &engine.config.working_dir,
    ) {
        Ok(store) => store,
        Err(e) => return vec![format!("cannot open memory store: {e}")],
    };
    let path = store.root().join("issues.jsonl");
    let mut issues: Vec<(bool, String)> = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let mut parts = l.splitn(2, '\u{1f}');
            let open = parts.next()? == "1";
            let text = parts.next()?.to_string();
            Some((open, text))
        })
        .collect();
    let arg = arg.trim();
    if let Some(text) = arg.strip_prefix("add ") {
        if !text.trim().is_empty() {
            issues.push((true, text.trim().to_string()));
            let body: String = issues
                .iter()
                .map(|(open, t)| {
                    format!("{}\u{1f}{t}\n", if *open { 1 } else { 0 })
                })
                .collect();
            let _ = std::fs::create_dir_all(store.root());
            if let Err(e) = std::fs::write(&path, body) {
                return vec![format!("cannot write issues: {e}")];
            }
            return vec![format!("issue added: {}", text.trim())];
        }
    }
    if let Some(idx) = arg.strip_prefix("done ") {
        if let Ok(i) = idx.trim().parse::<usize>() {
            if i > 0 && i <= issues.len() {
                issues[i - 1].0 = false;
                let body: String = issues
                    .iter()
                    .map(|(open, t)| {
                        format!("{}\u{1f}{t}\n", if *open { 1 } else { 0 })
                    })
                    .collect();
                let _ = std::fs::create_dir_all(store.root());
                if let Err(e) = std::fs::write(&path, body) {
                    return vec![format!("cannot write issues: {e}")];
                }
                return vec![format!("issue {i} closed")];
            }
        }
        return vec!["usage: /issues done <number>".into()];
    }
    if issues.is_empty() {
        return vec!["no issues — add one with /issues add <text>".into()];
    }
    let mut out = vec!["issues:".to_string()];
    for (i, (open, text)) in issues.iter().enumerate() {
        out.push(format!(
            "  {}. [{}] {text}",
            i + 1,
            if *open { "open" } else { "done" }
        ));
    }
    out
}

/// Blank the live region before committing lines into scrollback: every
/// `insert_before` scrolls the terminal up, and whatever is painted in the
/// viewport at that moment (streaming tail, working-status frame — even the
/// bordered input box) rides into scrollback as a stale duplicate. Drawing
/// an empty frame first makes the scrolled-up rows blank.
fn blank_frame(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
) -> anyhow::Result<()> {
    // An empty draw emits nothing; terminal.clear() actually erases the
    // viewport rows on screen and resets the back buffer, so the next
    // insert_before/draw starts from a consistent state.
    terminal.clear()?;
    Ok(())
}

/// Commit one line into native scrollback above the live viewport.
fn commit(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    line: &ratatui::text::Line<'static>,
) -> anyhow::Result<()> {
    commit_block(terminal, std::slice::from_ref(line))
}

/// Commit a block of lines into scrollback in ONE `insert_before` call.
/// Sequential 1-line inserts were the duplicated-answer bug: each insert
/// scrolls the screen up one row, pushing the painted viewport content
/// (the streaming tail) above the viewport one row at a time, where later
/// inserts' clears could no longer reach it. A single H-line insert
/// scrolls once and its non-sparse draw overwrites everything the scroll
/// pushed up, so nothing stale survives.
fn commit_block(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    lines: &[ratatui::text::Line<'static>],
) -> anyhow::Result<()> {
    use ratatui::widgets::Widget as _;
    if lines.is_empty() {
        return Ok(());
    }
    terminal.insert_before(lines.len() as u16, |buf| {
        for (i, line) in lines.iter().enumerate() {
            let row = ratatui::layout::Rect {
                x: 0,
                y: i as u16,
                width: buf.area.width,
                height: 1,
            };
            ratatui::widgets::Paragraph::new(line.clone()).render(row, buf);
        }
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
