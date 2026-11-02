//! The TUI run loop: fullscreen alternate screen + crossterm events +
//! engine task.
//!
//! Threading model: the render loop owns the terminal and redraws the full
//! screen each tick (~20fps — the alt screen makes whole-frame redraws the
//! simple, deterministic option; the scrollback is app state, so no
//! insert_before games). The engine runs on the Tokio runtime; deltas/tool
//! events arrive over std channels; approvals queue into a shared slot and
//! park the engine task via `tokio::task::block_in_place` until the user
//! decides.

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

/// Poll cadence for crossterm events (and the spinner tick).
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
    let kitty_keyboard = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    let mut stdout = std::io::stdout();
    crossterm::terminal::enable_raw_mode()?;
    // Bracketed paste (ctrl+V in most terminals delivers a paste event):
    // multi-line pastes enter the input instead of submitting. Mouse
    // capture: clicks position the input cursor (native text selection
    // still works with shift held — the terminal's own escape hatch).
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::EnableBracketedPaste,
        crossterm::event::EnableMouseCapture
    );
    if kitty_keyboard {
        use crossterm::event::{KeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
        let _ = crossterm::execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
    // Fullscreen: the alternate screen is the TUI's own canvas and the
    // scrollback lives INSIDE it (pgup/pgdn, wheel) — scrolling no longer
    // moves the whole terminal session. The earlier inline-viewport design
    // (Phase 1d) traded this away for native selection; that trade is
    // reversed by user direction.
    let _ = crossterm::terminal::Clear(crossterm::terminal::ClearType::All);
    let _ = crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen);
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;

    let mut app = App {
        upgrade_notice,
        session_id: resume_session,
        model_label: model_label.clone(),
        context_window: modron_router::lookup(&model_label)
            .map(|info| info.context_window)
            .unwrap_or(0),
        ..App::default()
    };

    // Welcome banner (Phase 6g): the TUI opens onto content, not a void —
    // now the first lines of the internal scrollback.
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
                    "  ·  enter send · shift/alt+enter newline · pgup/pgdn scroll · esc interrupt/live",
                    Style::default().fg(crate::DIM),
                ),
            ]),
            Line::from(""),
        ];
        app.push_history(welcome);
    }
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
                    app.cursor = app.input.chars().count();
                    continue;
                }
                crossterm::event::Event::Mouse(me) => match me.kind {
                    crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                        app.on_click(me.column, me.row)
                    }
                    crossterm::event::MouseEventKind::ScrollUp => app.scroll_history(3),
                    crossterm::event::MouseEventKind::ScrollDown => app.scroll_history(-3),
                    _ => {}
                },
                crossterm::event::Event::Key(key)
                    if key.kind == crossterm::event::KeyEventKind::Press =>
                {
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
                                        Some(chip) => {
                                            app.status_note = Some(format!(
                                                "attached {chip} (goes with your next message)"
                                            ))
                                        }
                                        None => {
                                            app.status_note =
                                                Some("clipboard image unreadable".into())
                                        }
                                    }
                                }
                                crate::clipboard::ClipboardContent::Text(text) => {
                                    let text = text.replace("\r\n", "\n").replace('\r', "\n");
                                    app.input.push_str(&text);
                                    app.cursor = app.input.chars().count();
                                }
                                crate::clipboard::ClipboardContent::Empty => {
                                    app.status_note = Some("clipboard empty".into());
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
                                                id,
                                                &task,
                                                attachments,
                                            )
                                            .await
                                    }
                                    None => {
                                        engine.run_task_with_attachments(&task, attachments).await
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
                            app.finish_interrupted();
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
                _ => {}
            }
        }

        // 3b. Slash commands needing engine/store powers (Phase 14): the
        //     App parks a PendingCommand; execute it here — the result
        //     lines land in the internal scrollback.
        if let Some(cmd) = app.pending_command.take() {
            for line in run_pending_command(&mut engine, &mut app, cmd).await {
                app.note(line);
            }
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
                    let swarm = app.swarm_summary();
                    app.push_history(swarm);
                    app.finish_turn(
                        summary.answer.clone(),
                        summary.usage,
                        summary.turns,
                        summary.tool_calls,
                        summary.session_id,
                    );
                }
                Outcome::Failed(e) => {
                    app.finish_interrupted();
                    let red = ratatui::style::Style::default().fg(ratatui::style::Color::Red);
                    app.push_history(vec![
                        ratatui::text::Line::from(format!("error: {e}")).style(red)
                    ]);
                }
                Outcome::Aborted => {}
            }
        }

        if app.quit {
            result = Ok(());
            break;
        }

        // 6. Render the full screen; advance the spinner.
        app.spinner_tick += 1;
        terminal.draw(|frame| app.view(frame, frame.area()))?;
    }

    // Teardown: restore the terminal, then print the session id into the
    // NORMAL buffer (it scrolls away with the alt screen otherwise).
    if kitty_keyboard {
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::event::PopKeyboardEnhancementFlags
        );
    }
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::DisableBracketedPaste,
        crossterm::event::DisableMouseCapture
    );
    let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen);
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
        // /auth — provider credential status. Reads the same env file the
        // CLI wizard writes (~/.config/modron/env); the file is the truth,
        // so no provider registry is duplicated here.
        PendingCommand::Auth => auth_status_lines(),
        PendingCommand::Agent(name) => {
            let name = name.trim().to_string();
            if name.is_empty() {
                let personas: Vec<String> = engine.config.personas.keys().cloned().collect();
                if personas.is_empty() {
                    vec!["no personas configured — define [agents.<name>] in config.toml".into()]
                } else {
                    vec![format!(
                        "personas: {} (switch: /agent <name>)",
                        personas.join(", ")
                    )]
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
                vec![format!(
                    "switched to persona `{name}` (model/provider follow if set)"
                )]
            } else {
                vec![format!(
                    "unknown persona `{name}` — configured: {}",
                    engine
                        .config
                        .personas
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
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
                Err(_) => vec!["no worktrees yet (created by delegate isolate:true)".to_string()],
            }
        }
        PendingCommand::Issues(arg) => run_issues(engine, &arg),
        PendingCommand::Fork => match app.session_id {
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
        },
    }
}

/// /issues — a tiny per-project board in the workspace memory store:
/// `<memory_root>/<slug>/issues.jsonl` with `open` state.
fn run_issues(engine: &Engine, arg: &str) -> Vec<String> {
    let Some(root) = engine.config.memory_root.clone() else {
        return vec!["memory not configured — issues live with memory".into()];
    };
    let store =
        match modron_core::memory::MemoryStore::for_workspace(&root, &engine.config.working_dir) {
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
                .map(|(open, t)| format!("{}\u{1f}{t}\n", if *open { 1 } else { 0 }))
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
                    .map(|(open, t)| format!("{}\u{1f}{t}\n", if *open { 1 } else { 0 }))
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

/// Compact /auth status: masked keys from the env file + the Claude OAuth
/// token file + the pointer to the CLI wizard.
fn auth_status_lines() -> Vec<String> {
    let mut out = vec!["provider credentials:".to_string()];
    let env_path = std::env::var("MODRON_CONFIG")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".to_string()))
                .join(".config")
                .join("modron")
        })
        .join("env");
    let mut any = false;
    if let Ok(content) = std::fs::read_to_string(&env_path) {
        for line in content.lines() {
            if let Some(rest) = line.strip_prefix("export ") {
                if let Some((var, value)) = rest.split_once('=') {
                    let value = value.trim().trim_matches('"');
                    if value.is_empty() {
                        continue;
                    }
                    any = true;
                    let n = value.chars().count();
                    let masked = if n > 8 {
                        let head: String = value.chars().take(3).collect();
                        let tail: String = value.chars().skip(n - 4).collect();
                        format!("{head}…{tail}")
                    } else {
                        "…".to_string()
                    };
                    out.push(format!("  {var} = {masked} (saved)"));
                }
            }
        }
    }
    for var in ["ZAI_API_KEY", "ANTHROPIC_API_KEY", "OPENAI_API_KEY"] {
        if let Ok(v) = std::env::var(var) {
            if !v.is_empty() && !out.iter().any(|l| l.starts_with(&format!("  {var}"))) {
                any = true;
                let n = v.chars().count();
                let tail: String = v.chars().skip(n.saturating_sub(4)).collect();
                out.push(format!("  {var} = …{tail} (process env)"));
            }
        }
    }
    if !any {
        out.push("  (none configured)".to_string());
    }
    let oauth = std::env::var("HOME")
        .map(|h| {
            std::path::PathBuf::from(h)
                .join(".modron")
                .join("auth")
                .join("anthropic.json")
        })
        .ok()
        .and_then(|p| std::fs::metadata(&p).ok().map(|_| p));
    if oauth.is_some() {
        out.push("  claude subscription: OAuth tokens present".to_string());
    }
    out.push(
        "add a provider: run `modron auth add` (or `modron auth set <provider> <key>`)".to_string(),
    );
    out
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
