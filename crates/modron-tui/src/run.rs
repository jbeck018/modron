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

use crate::{App, AuthFlow, PendingApproval, PendingCommand};
use modron_core::{
    ApprovalMode, Approver, DeltaSink, Engine, RunSummary, SessionId, StreamDelta, ToolActivity,
    ToolActivitySink, ToolCall,
};
use modron_protocol::{ConfigPatch, Decision, Frame, Payload, WireRunSummary};
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

/// Spawned OAuth flows report through this: an interim update renders in
/// the waiting modal (device flows deliver the URL + user code), Done
/// settles the flow with a note for the scrollback. Every message carries
/// the flow's generation — a cancelled flow's late messages must not
/// stomp a newer one's modal (seen live: a slow Claude failure wiped an
/// open ChatGPT waiting screen).
enum OAuthProgress {
    Interim {
        gen: u64,
        url: String,
        user_code: Option<String>,
    },
    Done {
        gen: u64,
        result: anyhow::Result<String>,
    },
}

#[derive(Clone)]
pub struct TuiConfig {
    pub resume: Option<SessionId>,
    pub model_label: String,
}

/// Remote-control connection (Phase 3): when set, the run loop drives a
/// `modron serve` process over the protocol instead of an in-process
/// engine. Sessions live on the server and survive this TUI closing.
#[derive(Clone, Debug)]
pub struct RemoteSpec {
    /// Connect target for [`crate::remote::RemoteClient`]: `"unix"`
    /// (default socket), `"unix:<path>"`, or `"ws://host:port"`.
    pub target: String,
    /// `None` = fresh session on first submit; `Some(None)` = the
    /// server's newest session (`--resume` bare); `Some(Some(id))` = that
    /// id (`--resume <id>`).
    pub resume: Option<Option<SessionId>>,
}

/// Loop -> driver commands (the driver owns the [`RemoteClient`]; the
/// render loop never blocks on the wire).
enum LoopCmd {
    Submit {
        session: Option<SessionId>,
        task: String,
        attachments: Vec<modron_core::Attachment>,
    },
    Interrupt {
        session: Option<SessionId>,
    },
    Approve {
        request_id: String,
        decision: modron_protocol::Decision,
    },
    SetConfig(modron_protocol::ConfigPatch),
    RemoteControl {
        bridge_url: String,
    },
}

/// Driver -> loop updates that do not fit the delta/approval/outcome
/// channels (drained each tick).
enum UiFrame {
    Note(String),
    // Boxed: AttachSnapshot carries a full event log + LiveState (392B+)
    // against a Note's 24B.
    Snapshot(Box<crate::remote::AttachSnapshot>),
    ConfigChanged(modron_protocol::ConfigPatch),
    Disconnected,
}

/// Poll cadence for crossterm events (and the spinner tick).
const TICK: Duration = Duration::from_millis(50);

/// Restores the terminal on EVERY exit path — a propagated `?` or a panic
/// past the render loop would otherwise leave the user's shell in raw mode
/// on the alternate screen. Idempotent.
struct TerminalRestore {
    kitty: bool,
    done: bool,
}

impl TerminalRestore {
    fn restore(&mut self) {
        if self.done {
            return;
        }
        self.done = true;
        if self.kitty {
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
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

impl Drop for TerminalRestore {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Shared approval slot: the approver queues the REAL ToolCall (local
/// closures clone the call they were handed; the remote driver carries the
/// wire's call verbatim); the render loop drains it into the App; the
/// response channel carries the decision.
type ApprovalSlot = Arc<Mutex<Option<(ToolCall, mpsc::Sender<bool>)>>>;

#[derive(Clone)]
struct ApprovalBus {
    slot: ApprovalSlot,
    rest_approved: Arc<std::sync::atomic::AtomicBool>,
}

/// Run the TUI. `build` receives the wired sinks and returns the engine plus
/// the session to resume (if `--resume` was given). Requires a TTY on stdout;
/// returns an actionable error otherwise. The closure is fallible so a failed
/// engine build surfaces as an error (terminal restored by the guard) instead
/// of a panic.
pub async fn run_with(
    build: impl FnOnce(TuiSinks) -> anyhow::Result<(Engine, Option<SessionId>)>,
    model_label: String,
    upgrade_notice: Option<String>,
    remote: Option<RemoteSpec>,
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
    let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel::<UiFrame>();

    // Remote-control connect happens BEFORE the terminal goes raw: a bad
    // target fails fast with a plain error, nothing to restore.
    let is_remote = remote.is_some();
    let remote_target = remote.as_ref().map(|spec| spec.target.clone());
    let mut connected = None;
    if let Some(spec) = remote {
        let mut client = crate::remote::RemoteClient::connect(&spec.target).await?;
        let session = match spec.resume {
            None => None,
            Some(want) => match want {
                None => client.list_sessions().await?.first().map(|meta| meta.id),
                Some(id) => Some(id),
            },
        };
        let snapshot = match session {
            Some(id) => Some(client.attach(id, 0).await?),
            None => None,
        };
        connected = Some((client, snapshot));
    }

    // The engine builder gets sinks that talk to THIS loop. Remote mode
    // never calls it — the engine lives on the server — and the loop's
    // local branches hold an inert stand-in so the code stays uniform.
    let (mut engine, resume_session) = if connected.is_some() {
        (inert_engine(), None)
    } else {
        let delta_sink_tx = delta_tx.clone();
        let tool_tx = delta_tx.clone();
        let approval_for_builder = approval.clone();
        build(TuiSinks {
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
                *approval_for_builder.slot.lock().unwrap() = Some((call.clone(), respond));
                // Park the engine task until the render loop delivers the
                // decision. block_in_place keeps other runtime workers alive.
                tokio::task::block_in_place(|| receiver.recv().unwrap_or(false))
            }),
        })?
    };

    // Terminal setup: inline viewport, raw mode, hidden cursor.
    // Kitty keyboard protocol (kitty/Ghostty/WezTerm/iTerm2 3.5+): lets the
    // terminal report modifiers on Enter, making shift+enter a real newline
    // instead of a submit. The query returns quickly on any terminal that
    // answers device attributes; unsupported ones ignore the push entirely.
    let kitty_keyboard = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    let mut stdout = std::io::stdout();
    crossterm::terminal::enable_raw_mode()?;
    // From here on, every exit path must restore the terminal — the guard's
    // Drop covers error returns and panics the inline teardown cannot.
    let mut terminal_restore = TerminalRestore {
        kitty: kitty_keyboard,
        done: false,
    };
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
    let _ = crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen);
    let _ = crossterm::execute!(
        stdout,
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All)
    );
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;

    // Remote attach state drives a few App fields the local path takes
    // from the engine (menus degrade gracefully: no personas/tool lists
    // remotely yet — the server does not expose them over the protocol).
    let attached = connected.as_ref().and_then(|(_, snap)| snap.as_ref());
    let mut app = App {
        upgrade_notice,
        session_id: attached.map(|snap| snap.session_id).or(resume_session),
        model_label: attached
            .and_then(|snap| (!snap.live.model.is_empty()).then(|| snap.live.model.clone()))
            .unwrap_or_else(|| model_label.clone()),
        context_window: modron_router::lookup(&model_label)
            .map(|info| info.context_window)
            .unwrap_or(0),
        // Menu inputs (personas, tool list, current model) come from the
        // engine the loop owns.
        personas: if is_remote {
            Vec::new()
        } else {
            engine.config.personas.keys().cloned().collect()
        },
        tool_names: if is_remote {
            Vec::new()
        } else {
            engine
                .tools
                .specs()
                .iter()
                .map(|s| s.name.clone())
                .collect()
        },
        current_model: attached
            .and_then(|snap| (!snap.live.model.is_empty()).then(|| snap.live.model.clone()))
            .unwrap_or_else(|| engine.config.default_model.clone()),
        current_theme: "modron".to_string(),
        // The status strip's mode label mirrors the engine's real state
        // (shift+tab and /mode keep it current through PendingCommand).
        approval_mode: if is_remote {
            "ask".to_string()
        } else {
            match engine.config.approvals.mode {
                ApprovalMode::Ask => "ask",
                ApprovalMode::ApproveAll => "yolo",
                ApprovalMode::Deny => "deny",
            }
            .to_string()
        },
        working_dir: if is_remote {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        } else {
            engine.config.working_dir.clone()
        },
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
    // Remote banner note + attach replay: the persisted transcript lands
    // in scrollback (same styles as the live path) before anything runs.
    if let Some(target) = remote_target.as_deref() {
        app.note(format!(
            "connected to {target} — sessions live on the server and survive this TUI"
        ));
    }
    if let Some((_, Some(snap))) = connected.as_ref() {
        if !snap.events.is_empty() {
            app.backfill_transcript(modron_view::replay(&snap.events));
        }
    }
    let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel::<Outcome>();
    // OAuth progress from the spawned flows: the interim update shows the
    // URL (+ device-flow user code) in the modal, Done settles it.
    let (oauth_tx, mut oauth_rx) = tokio::sync::mpsc::unbounded_channel::<OAuthProgress>();
    let mut oauth_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut oauth_generation: u64 = 0;
    // Slow slash commands (/tasks, /fork) run OFF the render loop — same
    // spawn + channel + generation pattern as the OAuth flows — so redraws
    // and Esc keep working while they await tools/store I/O.
    let (slow_tx, mut slow_rx) = tokio::sync::mpsc::unbounded_channel::<SlowCommandProgress>();
    let mut slow_command_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut slow_command_generation: u64 = 0;
    // Clipboard I/O (pbcopy/wl-copy/osascript subprocesses) is also async.
    let (clip_tx, mut clip_rx) = tokio::sync::mpsc::unbounded_channel::<ClipboardProgress>();
    let mut harvest_task: Option<tokio::task::JoinHandle<()>> = None;
    // `!` shell passthrough is async too: a slow `make` must never freeze
    // the render loop, and Esc gets to kill it (kill_on_drop).
    let (shell_tx, mut shell_rx) = tokio::sync::mpsc::unbounded_channel::<ShellOutcome>();
    let mut shell_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut engine_task: Option<tokio::task::JoinHandle<()>> = None;
    // Remote mode's one-at-a-time flag (no JoinHandle exists to check).
    let mut remote_busy = false;
    // The first snapshot with events replays the transcript; later ones
    // (auto-attach echoes) must not duplicate history.
    let mut backfilled = false;
    #[allow(unused_assignments)]
    let mut result: anyhow::Result<()> = Ok(());

    // Remote-control driver: owns the client, translates frames into the
    // loop's channels. The loop talks to it through loop_cmd_tx.
    let (loop_cmd_tx, loop_cmd_rx) = tokio::sync::mpsc::unbounded_channel::<LoopCmd>();
    if let Some((client, _)) = connected.take() {
        spawn_remote_driver(
            client,
            loop_cmd_rx,
            loop_cmd_tx.clone(),
            delta_tx.clone(),
            done_tx.clone(),
            ui_tx.clone(),
            approval.clone(),
        );
    }

    loop {
        // 1. Drain engine deltas/tool events.
        while let Ok(msg) = delta_rx.try_recv() {
            match msg {
                DeltaOrTool::Delta(d) => app.on_delta(d),
                DeltaOrTool::Tool(t) => app.on_tool(t),
            }
        }

        // 2. Drain the approval slot into the App (redraw happens below).
        //     The slot carries the REAL ToolCall (remote approvals arrive
        //     with one; the local approver clones the one it was given) —
        //     the old summary-string reconstruction is gone.
        if app.pending_approval.is_none() {
            if let Some((call, respond)) = approval.slot.lock().unwrap().take() {
                app.pending_approval = Some(PendingApproval { call, respond });
            }
        }

        // 3. Poll keyboard (raw mode).
        if crossterm::event::poll(TICK)? {
            match crossterm::event::read()? {
                crossterm::event::Event::Paste(text) => {
                    // Bracketed paste: multi-line text enters the input
                    // verbatim — the terminal told us it is a paste, so it
                    // must never submit. The auth wizard's key entry takes
                    // precedence when open.
                    let text = text.replace("\r\n", "\n").replace('\r', "\n");
                    app.paste_into_auth_or_input(&text);
                    continue;
                }
                crossterm::event::Event::Mouse(me) => match me.kind {
                    crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                        app.on_click(me.column, me.row)
                    }
                    crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
                        app.extend_selection(me.column, me.row)
                    }
                    crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
                        // Selection release: copy asynchronously — the
                        // clipboard helper shells out (pbcopy/wl-copy), and
                        // the render loop must not wait on it.
                        if let Some(text) = app.take_selected_text() {
                            let chars = text.chars().count();
                            app.toast(format!("copying {chars} chars…"));
                            let tx = clip_tx.clone();
                            tokio::spawn(async move {
                                let ok = crate::clipboard::copy_text(&text);
                                let _ = tx.send(ClipboardProgress::Copied { ok, chars });
                            });
                        }
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
                            // the input. Spawned (bounded by the harvest
                            // timeout inside) so the render loop never
                            // waits on a clipboard service.
                            if harvest_task.is_some() {
                                app.toast("clipboard read already in flight");
                                continue;
                            }
                            app.toast("reading clipboard…");
                            let tx = clip_tx.clone();
                            harvest_task = Some(tokio::spawn(async move {
                                let content =
                                    tokio::task::spawn_blocking(crate::clipboard::harvest)
                                        .await
                                        .unwrap_or(crate::clipboard::ClipboardContent::Empty);
                                let _ = tx.send(ClipboardProgress::Harvested(content));
                            }));
                        }
                        crate::Action::Submit(task) => {
                            // `!` shell passthrough: the user typed it
                            // themselves, so no approval gate — and it
                            // never reaches the model. Echo the command in
                            // accent, run it in the workspace (spawned; the
                            // render loop keeps drawing), land the captured
                            // output as dim history lines.
                            if let Some(command) = shell_passthrough(&task) {
                                if command.is_empty() {
                                    app.toast("usage: !<shell command>");
                                    continue;
                                }
                                if shell_task.is_some() {
                                    app.toast("a shell command is still running — Esc cancels it");
                                    continue;
                                }
                                let accent = ratatui::style::Style::default()
                                    .fg(app.theme.resolved().accent);
                                app.push_history(vec![ratatui::text::Line::from(format!(
                                    "❯ !{command}"
                                ))
                                .style(accent)]);
                                let tx = shell_tx.clone();
                                let dir = if is_remote {
                                    // `!` runs on the client machine; the
                                    // server's working dir is its own.
                                    std::env::current_dir()
                                        .unwrap_or_else(|_| std::path::PathBuf::from("."))
                                } else {
                                    engine.config.working_dir.clone()
                                };
                                shell_task = Some(tokio::spawn(async move {
                                    let result = tokio::process::Command::new("sh")
                                        .arg("-c")
                                        .arg(&command)
                                        .current_dir(&dir)
                                        // Esc-abort kills the child, not
                                        // just the awaiting task.
                                        .kill_on_drop(true)
                                        .output()
                                        .await;
                                    let _ = tx.send(ShellOutcome { command, result });
                                }));
                                continue;
                            }
                            // One engine task at a time: a second concurrent
                            // spawn would overwrite the first JoinHandle
                            // (unabortable orphan streaming into the App,
                            // double finish_turn, clobbered approval slot).
                            if engine_task.is_some() || remote_busy {
                                app.toast("a turn is already running — Esc aborts it");
                                continue;
                            }
                            let task = app.task_with_goal(&task);
                            app.begin_task(&task);
                            let attachments = app.take_attachments();
                            if is_remote {
                                // Server-authoritative: the session id
                                // arrives on the auto-attach Snapshot
                                // (first submit) or is already known;
                                // TurnDone/Aborted clear the busy flag.
                                let _ = loop_cmd_tx.send(LoopCmd::Submit {
                                    session: app.session_id,
                                    task,
                                    attachments,
                                });
                                remote_busy = true;
                                continue;
                            }
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
                            if is_remote {
                                // Server-authoritative interrupt: TurnAborted
                                // drives finish_interrupted (a dead server
                                // leaves the turn running rather than a fake
                                // local abort).
                                if remote_busy {
                                    let _ = loop_cmd_tx.send(LoopCmd::Interrupt {
                                        session: app.session_id,
                                    });
                                }
                            } else {
                                match (engine_task.take(), slow_command_task.take()) {
                                    (Some(task), _) => {
                                        task.abort();
                                        // Give the runtime a beat to cancel.
                                        tokio::time::sleep(Duration::from_millis(30)).await;
                                        app.finish_interrupted();
                                    }
                                    // No engine running: Esc cancels a slow
                                    // command instead. Bumping the generation
                                    // drops any Done already sitting in the
                                    // channel (an aborted /fork must not adopt
                                    // its new session id).
                                    (None, Some(task)) => {
                                        slow_command_generation += 1;
                                        task.abort();
                                        app.toast("command cancelled");
                                    }
                                    (None, None) => {}
                                }
                            }
                            // A pending `!` shell job cancels with Esc too
                            // (kill_on_drop kills the child process).
                            if let Some(task) = shell_task.take() {
                                task.abort();
                                app.toast("shell command cancelled");
                            }
                        }
                        crate::Action::Quit => {
                            if let Some(task) = engine_task.take() {
                                task.abort();
                            }
                            if let Some(task) = slow_command_task.take() {
                                slow_command_generation += 1;
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
        //     lines land in the internal scrollback. The OAuth login is
        //     the exception: it waits on a browser round-trip, so it runs
        //     as a spawned task (the TUI keeps rendering; Esc on the modal
        //     cancels it).
        // NOTE: peek with matches! before take() — an `if let` over
        // `.take()` drops the value when the pattern fails.
        if is_remote {
            // Connected: config slash commands become SetConfig patches;
            // anything needing the local engine is refused EXPLICITLY
            // (OAuth flows, /tasks, /fork run on the machine that owns
            // the engine — the server). /remote-control is the exception
            // that is remote-NATIVE: it opens a tunnel through the bridge
            // so any device can watch and drive this session.
            if let Some(cmd) = app.pending_command.take() {
                match cmd {
                    PendingCommand::RemoteControl { url } => {
                        if url.is_empty() {
                            app.toast("usage: /remote-control <bridge url> (deploy cloud/bridge)");
                        } else {
                            let _ = loop_cmd_tx.send(LoopCmd::RemoteControl { bridge_url: url });
                        }
                    }
                    other => match remote_slash_command(&other) {
                        Some((patch, mode_label)) => {
                            if let Some(label) = mode_label {
                                app.approval_mode = label;
                            }
                            let _ = loop_cmd_tx.send(LoopCmd::SetConfig(patch));
                        }
                        None => {
                            app.toast("needs the local engine — restart without --connect");
                        }
                    },
                }
            }
        } else if matches!(app.pending_command, Some(PendingCommand::OAuthLogin { .. })) {
            let Some(PendingCommand::OAuthLogin { session }) = app.pending_command.take() else {
                unreachable!("matches! guard above")
            };
            oauth_generation += 1;
            let gen = oauth_generation;
            let tx = oauth_tx.clone();
            oauth_task = Some(tokio::spawn(async move {
                let result: anyhow::Result<String> = async {
                    // Binds the listener FIRST, then opens the browser, so
                    // an instant approval can never race the socket.
                    let code = modron_core::auth::oauth::wait_for_callback(
                        session.port,
                        &session.state,
                        Some(&session.url),
                    )
                    .await?;
                    let tokens = modron_core::auth::oauth::exchange_code(&session, &code).await?;
                    modron_core::auth::oauth::save_tokens(&tokens)?;
                    Ok(format!(
                        "✓ claude subscription tokens saved ({}) — `modron auth status`",
                        modron_core::auth::mask_key(&tokens.access_token)
                    ))
                }
                .await;
                let _ = tx.send(OAuthProgress::Done { gen, result });
            }));
        } else if matches!(app.pending_command, Some(PendingCommand::ZaiLogin { .. })) {
            let Some(PendingCommand::ZaiLogin { session }) = app.pending_command.take() else {
                unreachable!("matches! guard above")
            };
            oauth_generation += 1;
            let gen = oauth_generation;
            let tx = oauth_tx.clone();
            oauth_task = Some(tokio::spawn(async move {
                let result: anyhow::Result<String> = async {
                    // Listener first, then the browser (same race rule as
                    // the Claude flow).
                    let code = modron_core::auth::oauth::wait_for_callback(
                        session.port,
                        &session.state,
                        Some(&session.url),
                    )
                    .await?;
                    let (key, email) =
                        modron_core::auth::oauth::zai::finish(&code, &session.state, session.port)
                            .await?;
                    // The minted id.secret key IS a ZAI_API_KEY: save it to
                    // the shared env file so every launch picks it up.
                    let path = modron_core::auth::env_file()
                        .map_err(|e| anyhow::anyhow!("cannot resolve env file: {e}"))?;
                    modron_core::auth::set_key_in_env_file(&path, "ZAI_API_KEY", &key)
                        .map_err(|e| anyhow::anyhow!("cannot save key: {e}"))?;
                    std::env::set_var("ZAI_API_KEY", &key);
                    Ok(format!(
                        "✓ z.ai signed in{} — GLM Coding Plan key saved ({}) — use `--provider zai`",
                        email.map(|e| format!(" ({e})")).unwrap_or_default(),
                        modron_core::auth::mask_key(&key)
                    ))
                }
                .await;
                let _ = tx.send(OAuthProgress::Done { gen, result });
            }));
        } else if matches!(app.pending_command, Some(PendingCommand::ChatGptLogin)) {
            app.pending_command = None;
            oauth_generation += 1;
            let gen = oauth_generation;
            let tx = oauth_tx.clone();
            oauth_task = Some(tokio::spawn(async move {
                let result: anyhow::Result<String> = async {
                    let device = modron_core::auth::oauth::chatgpt::start().await?;
                    // The verification page opens itself; the modal keeps
                    // the URL as the manual fallback.
                    modron_core::auth::open_browser(&device.verification_uri);
                    let _ = tx.send(OAuthProgress::Interim {
                        gen,
                        url: device.verification_uri.clone(),
                        user_code: Some(device.user_code.clone()),
                    });
                    let (tokens, account) =
                        modron_core::auth::oauth::chatgpt::finish(&device).await?;
                    modron_core::auth::oauth::chatgpt::save(
                        &modron_core::auth::oauth::chatgpt::ChatGptAuth {
                            access_token: tokens.access_token.clone(),
                            refresh_token: tokens.refresh_token.clone(),
                            account_id: account,
                        },
                    )?;
                    Ok(format!(
                        "✓ chatgpt signed in ({}) — use `--provider chatgpt` (models gpt-6-astra, gpt-5.6-sol)",
                        modron_core::auth::mask_key(&tokens.access_token)
                    ))
                }
                .await;
                let _ = tx.send(OAuthProgress::Done { gen, result });
            }));
        } else if app.pending_command.as_ref().is_some_and(is_slow_command)
            && slow_command_task.is_none()
            && engine_task.is_none()
        {
            // Slow commands (/tasks runs a tool, /fork rewrites the whole
            // session log) spawn instead of blocking the render loop.
            let Some(cmd) = app.pending_command.take() else {
                unreachable!("guarded by is_some_and above")
            };
            slow_command_generation += 1;
            let gen = slow_command_generation;
            let engine_for_task = engine.clone();
            let tx = slow_tx.clone();
            let session = app.session_id;
            app.toast(format!("/{} running (Esc cancels)…", cmd_label(&cmd)));
            slow_command_task = Some(tokio::spawn(async move {
                let result = run_slow_pending_command(engine_for_task, cmd, session).await;
                let _ = tx.send(SlowCommandProgress::Done { gen, result });
            }));
        } else if let Some(cmd) = app.pending_command.take() {
            // Keep the strip's mode label honest with the engine's real
            // state: /mode's arg is normalized by run_pending_command, so
            // mirror the same acceptance (invalid args change nothing).
            if let PendingCommand::Mode(m) = &cmd {
                match m.trim() {
                    "ask" => app.approval_mode = "ask".into(),
                    "yolo" | "auto" => app.approval_mode = "yolo".into(),
                    _ => {}
                }
            }
            for line in run_pending_command(&mut engine, cmd).await {
                app.note(line);
            }
        }

        // 3c. OAuth progress: interim updates render in the waiting modal,
        //     Done settles it. The spawned flow is aborted whenever the
        //     waiting screen isn't up (modal closed OR backed out to
        //     another manager screen). Messages from an older
        //     (cancelled/replaced) flow generation are dropped — they must
        //     not stomp the current modal.
        if !matches!(app.auth_flow, Some(AuthFlow::WaitingOAuth { .. })) {
            if let Some(task) = oauth_task.take() {
                task.abort();
            }
        }
        while let Ok(progress) = oauth_rx.try_recv() {
            let stale = match &progress {
                OAuthProgress::Interim { gen, .. } | OAuthProgress::Done { gen, .. } => {
                    *gen != oauth_generation
                }
            };
            if stale {
                continue;
            }
            match progress {
                OAuthProgress::Interim { url, user_code, .. } => {
                    // Replace url/user_code, keep the provider info the
                    // modal was opened with. NOTE: match — NOT `if let` over
                    // `.take()`: a current-gen Interim can arrive after the
                    // user Esc'd back to an earlier manager screen, and
                    // dropping the taken flow would close the whole manager.
                    match app.auth_flow.take() {
                        Some(AuthFlow::WaitingOAuth { info, .. }) => {
                            app.auth_flow = Some(AuthFlow::WaitingOAuth {
                                info,
                                url,
                                user_code,
                            });
                        }
                        other => app.auth_flow = other,
                    }
                }
                OAuthProgress::Done { result, .. } => {
                    oauth_task = None;
                    // Only settle the modal when it is still up — a late
                    // Done must not close a manager the user deliberately
                    // backed out of (Esc keeps the manager open).
                    if matches!(app.auth_flow, Some(AuthFlow::WaitingOAuth { .. })) {
                        app.auth_flow = None;
                    }
                    match result {
                        Ok(note) => app.note(note),
                        Err(e) => app.note(format!("oauth failed: {e}")),
                    }
                }
            }
        }

        // 3d. Slow-command and clipboard results land here (spawned above);
        //     stale generations (aborted/replaced commands) are dropped.
        while let Ok(progress) = slow_rx.try_recv() {
            let SlowCommandProgress::Done { gen, result } = progress;
            if gen != slow_command_generation {
                continue;
            }
            slow_command_task = None;
            for line in result.lines {
                app.note(line);
            }
            if let Some(id) = result.new_session {
                app.session_id = Some(id);
            }
        }
        while let Ok(progress) = clip_rx.try_recv() {
            if matches!(progress, ClipboardProgress::Harvested(_)) {
                harvest_task = None;
            }
            apply_clipboard_progress(&mut app, progress);
        }

        // 3e. `!` shell passthrough result: stdout, stderr and the exit
        //     code land as dim lines under the accent echo, bounded like
        //     the bash tool's capture — a runaway `cat` must not flood
        //     the scrollback.
        while let Ok(outcome) = shell_rx.try_recv() {
            shell_task = None;
            match outcome.result {
                Ok(output) => {
                    let stdout = cap_output(String::from_utf8_lossy(&output.stdout).trim_end());
                    let stderr = cap_output(String::from_utf8_lossy(&output.stderr).trim_end());
                    if !stdout.is_empty() {
                        app.note(stdout);
                    }
                    if !stderr.is_empty() {
                        app.note(stderr);
                    }
                    match output.status.code() {
                        Some(0) => app.note("exit 0"),
                        Some(code) => app.note(format!("exit {code}")),
                        None => app.note("exit: killed by signal"),
                    }
                }
                Err(e) => app.note(format!("!{}: {e}", outcome.command)),
            }
        }

        // 3f. Remote-control updates: notes (server toasts/slash output,
        //     oauth progress), config echoes (mode/model labels), and
        //     auto-attach snapshots (the session id arrives here on a
        //     first submit to a fresh session).
        while let Ok(frame) = ui_rx.try_recv() {
            match frame {
                UiFrame::Note(text) => app.note(text),
                UiFrame::ConfigChanged(patch) => {
                    if let Some(mode) = patch.approval_mode {
                        app.approval_mode = match mode {
                            modron_protocol::ApprovalModeWire::ApproveAll => "yolo".into(),
                            modron_protocol::ApprovalModeWire::Deny => "deny".into(),
                            modron_protocol::ApprovalModeWire::Ask => "ask".into(),
                        };
                    }
                    if let Some(model) = patch.model {
                        app.model_label = model.clone();
                        app.current_model = model;
                    }
                }
                UiFrame::Snapshot(snap) => {
                    app.session_id = Some(snap.session_id);
                    if !snap.events.is_empty() && !backfilled {
                        backfilled = true;
                        app.backfill_transcript(modron_view::replay(&snap.events));
                    }
                    if !snap.live.model.is_empty() && app.current_model != snap.live.model {
                        app.model_label = snap.live.model.clone();
                        app.current_model = snap.live.model.clone();
                    }
                }
                UiFrame::Disconnected => {
                    remote_busy = false;
                    app.toast("server disconnected — restart with --connect to reattach");
                }
            }
        }

        // 4. Approve-rest mirror: when the user pressed `a`, future approvals
        //    short-circuit in the approver; drain any queued one so an
        //    approval parked BEFORE the `a` press is answered, not re-prompted.
        if app.approve_rest {
            approval
                .rest_approved
                .store(true, std::sync::atomic::Ordering::Relaxed);
            if let Some((_, respond)) = approval.slot.lock().unwrap().take() {
                let _ = respond.send(true);
            }
        }

        // 5. Engine settled? (Re-drain the event channels first: deltas and
        // tool activities can queue behind the Done notification.)
        while let Ok(outcome) = done_rx.try_recv() {
            engine_task = None;
            remote_busy = false;
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
                Outcome::Aborted => {
                    // Remote interrupts land here (server-authoritative);
                    // local aborts finish inline before any frame could.
                    app.finish_interrupted();
                }
            }
        }

        if app.quit {
            result = Ok(());
            break;
        }

        // 6. Render the full screen; advance the spinner and age the toast.
        app.tick();
        terminal.draw(|frame| app.view(frame, frame.area()))?;
    }

    // Teardown (guard-backed): restore the terminal from the normal path
    // too, then print the session id into the NORMAL buffer (it scrolls
    // away with the alt screen otherwise).
    terminal_restore.restore();
    // A /fork that landed during the final tick still adopts its session.
    while let Ok(SlowCommandProgress::Done { gen, result }) = slow_rx.try_recv() {
        if gen == slow_command_generation {
            if let Some(id) = result.new_session {
                app.session_id = Some(id);
            }
        }
    }
    if let Some(id) = app.session_id {
        println!("session: {id} — resume with --resume {id}");
    }
    result
}

/// Execute a parked FAST slash command against the engine. Returns the
/// lines to commit. Engine mutations apply to every FUTURE turn (each
/// submit clones the engine). Slow commands (/tasks, /fork) are spawned
/// by the loop instead — see [`run_slow_pending_command`].
async fn run_pending_command(engine: &mut Engine, cmd: PendingCommand) -> Vec<String> {
    match cmd {
        // Remote mode intercepts /remote-control before the local
        // dispatch; unreachable here, but the match must stay total.
        // /remote-control only works over a serve connection: in the
        // local path say so explicitly instead of ignoring. Automations
        // and publish run on the server too.
        PendingCommand::RemoteControl { .. }
        | PendingCommand::Automations
        | PendingCommand::Publish => {
            vec!["needs a server session - run `modron serve`, connect with \
             --connect, then /remote-control <bridge url>"
                .to_string()]
        }
        // /auth — provider credentials against the shared core store
        // (`modron_core::auth`): bare = status, `<provider> <key>` =
        // save, `remove <provider>` = forget. The key never reaches the
        // model — slash commands are handled entirely locally.
        PendingCommand::Auth(arg) => run_auth_command(&arg),
        // The interactive flow's key: same store path as `/auth <p> <k>`,
        // with the key echoed masked only.
        PendingCommand::AuthSave { name, env_var, key } => {
            match modron_core::auth::env_file()
                .map_err(|e| e.to_string())
                .and_then(|p| {
                    modron_core::auth::set_key_in_env_file(&p, &env_var, &key)
                        .map_err(|e| e.to_string())
                }) {
                Ok(()) => {
                    std::env::set_var(&env_var, &key);
                    vec![format!(
                        "✓ {name} key saved ({}) — ready; takes effect on your next launch if already running",
                        modron_core::auth::mask_key(&key)
                    )]
                }
                Err(e) => vec![format!("cannot save key: {e}")],
            }
        }
        // The interactive flow's "remove saved credentials": forget the
        // key and any stored OAuth sign-in tokens.
        PendingCommand::AuthRemove {
            name,
            env_var,
            provider_id,
        } => {
            let mut lines = Vec::new();
            match modron_core::auth::env_file()
                .map_err(|e| e.to_string())
                .and_then(|p| {
                    modron_core::auth::remove_key_from_env_file(&p, &env_var)
                        .map_err(|e| e.to_string())
                }) {
                Ok(true) => {
                    std::env::remove_var(&env_var);
                    lines.push(format!("✓ {name} key removed"));
                }
                Ok(false) => lines.push(format!("no saved {name} key")),
                Err(e) => lines.push(format!("cannot remove key: {e}")),
            }
            if modron_core::auth::oauth_capable(&provider_id) {
                // Each OAuth sign-in has its own token store — removing
                // provider X must never delete provider Y's tokens (zai's
                // env-file key is handled above; it has no token file).
                let token_file: Result<Option<std::path::PathBuf>, _> = match provider_id.as_str() {
                    "anthropic" => modron_core::auth::oauth::auth_file().map(Some),
                    "chatgpt" => modron_core::auth::oauth::chatgpt::token_file().map(Some),
                    _ => Ok(None),
                };
                if let Ok(Some(path)) = token_file {
                    if path.exists() {
                        match std::fs::remove_file(&path) {
                            Ok(()) => lines.push(format!(
                                "✓ {name} sign-in tokens removed ({})",
                                path.display()
                            )),
                            Err(e) => lines.push(format!("cannot remove sign-in tokens: {e}")),
                        }
                    }
                }
            }
            lines
        }
        // /model: the engine's own default switches here (each submit
        // clones it, so the change lands from the next turn on).
        PendingCommand::Model { name, save_default } => {
            // The PROVIDER follows the model, and the SELECTOR is pinned to
            // the choice — the router would otherwise keep routing to its
            // own cheap/strong tiers (the live bug: chatgpt-lineup names
            // were sent to the anthropic endpoint).
            engine.config.default_model = name.clone();
            if let Some(info) = modron_router::lookup(&name) {
                engine.config.default_provider = info.provider.clone();
            }
            engine.selector = std::sync::Arc::new(modron_core::testing::StaticSelector::new(
                &name,
                engine.config.default_provider.clone(),
            ));
            let detail = modron_router::lookup(&name).map(|m| {
                format!(
                    " — provider {}, {}k window",
                    m.provider,
                    m.context_window / 1000
                )
            });
            let mut lines = vec![match &detail {
                Some(d) => format!("model: {name}{d} (applies from the next turn)"),
                None => {
                    format!("model: {name} (unknown to the catalog; applies from the next turn)")
                }
            }];
            if save_default {
                match crate::save_default_model(&name, engine.config.default_provider.as_str()) {
                    Ok(path) => lines.push(format!(
                        "✓ saved as default ({}); future launches start here",
                        path.display()
                    )),
                    Err(e) => lines.push(format!("cannot save default: {e}")),
                }
            }
            lines
        }
        // /sandbox: switch the policy for future tool calls.
        // Z.ai sign-in never reaches this queue (the OAuth spawn arm
        // consumes it), but the match must stay exhaustive.
        PendingCommand::ZaiLogin { .. } => Vec::new(),
        PendingCommand::Sandbox(mode) => match mode.as_str() {
            "off" => {
                engine.config.sandbox = modron_core::SandboxPolicy::Off;
                vec!["sandbox: off (approval gates only)".into()]
            }
            "seatbelt" => {
                engine.config.sandbox = modron_core::SandboxPolicy::Seatbelt;
                vec!["sandbox: seatbelt — applies to future tool calls".into()]
            }
            "bubblewrap" => {
                engine.config.sandbox = modron_core::SandboxPolicy::Bubblewrap;
                vec!["sandbox: bubblewrap — applies to future tool calls".into()]
            }
            other => vec![format!(
                "usage: /sandbox off|seatbelt|bubblewrap (got {other:?})"
            )],
        },
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
        // OAuthLogin/ChatGptLogin never reach here — the run loop spawns
        // them before run_pending_command is called (they wait on browser
        // round-trips). Tasks/Fork are likewise spawned (is_slow_command).
        PendingCommand::OAuthLogin { .. } | PendingCommand::ChatGptLogin => Vec::new(),
        PendingCommand::Tasks | PendingCommand::Fork => Vec::new(), // spawned by the loop (is_slow_command)
    }
}

/// Whether a pending command does slow I/O (tool execution, whole-log
/// session copies) and must run on a spawned task instead of the render
/// loop. Exhaustive by intent: adding a slow variant must update this.
fn is_slow_command(cmd: &PendingCommand) -> bool {
    matches!(cmd, PendingCommand::Tasks | PendingCommand::Fork)
}

/// The /command label for status notes.
fn cmd_label(cmd: &PendingCommand) -> &'static str {
    match cmd {
        PendingCommand::Tasks => "tasks",
        PendingCommand::Fork => "fork",
        _ => "command",
    }
}

/// Result of a slow slash command: lines to commit, plus (for /fork) the
/// newly active session id to adopt.
#[derive(Default)]
struct SlowCommandResult {
    lines: Vec<String>,
    new_session: Option<SessionId>,
}

enum SlowCommandProgress {
    Done { gen: u64, result: SlowCommandResult },
}

/// The spawned bodies of the slow commands (moved verbatim from the old
/// inline arms — the engine clone shares the live subagent registry).
async fn run_slow_pending_command(
    engine: Engine,
    cmd: PendingCommand,
    session: Option<SessionId>,
) -> SlowCommandResult {
    match cmd {
        PendingCommand::Tasks => {
            // Run the delegate_status tool directly through the registry.
            let ctx = modron_core::ToolCtx {
                sandbox: engine.config.sandbox,
                working_dir: engine.config.working_dir.clone(),
                engine: Some(std::sync::Arc::new(engine.clone())),
            };
            let lines = match engine.tools.get("delegate_status") {
                Some(tool) => match tool.execute(serde_json::json!({}), &ctx).await {
                    Ok(outcome) => vec![outcome.content],
                    Err(e) => vec![format!("delegate_status failed: {e}")],
                },
                None => vec!["delegate_status tool not registered in this build".into()],
            };
            SlowCommandResult {
                lines,
                new_session: None,
            }
        }
        PendingCommand::Fork => match session {
            Some(id) => match engine.store.fork(id).await {
                Ok(new) => SlowCommandResult {
                    lines: vec![format!(
                        "forked: new branch {new} (active now; the original is untouched — \
                             resume it with --resume {id})"
                    )],
                    new_session: Some(new),
                },
                Err(e) => SlowCommandResult {
                    lines: vec![format!("fork failed: {e}")],
                    new_session: None,
                },
            },
            None => SlowCommandResult {
                lines: vec!["no session yet — send a task first, then /fork".into()],
                new_session: None,
            },
        },
        // Everything else is fast-local and never routed here.
        _ => SlowCommandResult::default(),
    }
}

/// Clipboard results applied on the render thread (the bodies of the old
/// inline ctrl+V / mouse-up handlers).
enum ClipboardProgress {
    Harvested(crate::clipboard::ClipboardContent),
    Copied { ok: bool, chars: usize },
}

fn apply_clipboard_progress(app: &mut App, progress: ClipboardProgress) {
    match progress {
        ClipboardProgress::Harvested(crate::clipboard::ClipboardContent::Image(path)) => {
            match app.attach_image(&path) {
                Some(chip) => app.toast(format!("attached {chip} (goes with your next message)")),
                None => app.toast("clipboard image unreadable"),
            }
        }
        ClipboardProgress::Harvested(crate::clipboard::ClipboardContent::Text(text)) => {
            let text = text.replace("\r\n", "\n").replace('\r', "\n");
            app.paste_into_auth_or_input(&text);
        }
        ClipboardProgress::Harvested(crate::clipboard::ClipboardContent::Empty) => {
            app.toast("clipboard empty");
        }
        ClipboardProgress::Copied { ok: true, chars } => {
            app.toast(format!("copied {chars} chars"));
        }
        ClipboardProgress::Copied { ok: false, .. } => {
            app.toast("selection made — no clipboard tool found");
        }
    }
}

/// `!command` detection for the submit path: everything after the leading
/// `!` runs in the shell — it never reaches the model. None = a normal
/// task; `Some("")` = bare `!` (the caller shows usage).
#[doc(hidden)]
pub fn shell_passthrough(input: &str) -> Option<String> {
    input.strip_prefix('!').map(|rest| rest.trim().to_string())
}

/// Output discipline for `!` passthrough, mirroring the bash tool's
/// MAX_BYTES: keep the TAIL (errors land at the end of a command's
/// output), head-mark the cut, never split a UTF-8 char. Empty in, empty out.
#[doc(hidden)]
pub fn cap_output(text: &str) -> String {
    const MAX: usize = 10 * 1024; // modron-tools bash MAX_BYTES
    if text.len() <= MAX {
        return text.to_string();
    }
    let mut start = text.len() - MAX;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("… (output truncated)\n{}", &text[start..])
}

/// A finished `!` passthrough: the command plus its captured output (or
/// the spawn failure).
struct ShellOutcome {
    command: String,
    result: std::io::Result<std::process::Output>,
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

/// `/auth` — status by default; `/auth <provider> <key>` saves;
/// `/auth set <provider> <key>` and `/auth remove <provider>` explicit
/// forms. Writes go to the shared env file; the new key applies to
/// future launches (the running process also picks it up).
fn run_auth_command(arg: &str) -> Vec<String> {
    use modron_core::auth as store;
    let arg = arg.trim();

    if let Some(rest) = arg
        .strip_prefix("remove ")
        .or_else(|| arg.strip_prefix("logout "))
    {
        let id = rest.trim();
        return match store::provider_info(id) {
            Some(info) => {
                match store::env_file().map_err(|e| e.to_string()).and_then(|p| {
                    store::remove_key_from_env_file(&p, info.env_var).map_err(|e| e.to_string())
                }) {
                    Ok(true) => vec![format!("✓ {} key removed", info.name)],
                    Ok(false) => vec![format!("no saved {} key", info.name)],
                    Err(e) => vec![format!("cannot remove key: {e}")],
                }
            }
            None => vec![unknown_provider_line(id)],
        };
    }

    // `/auth status` / `show`: the full text board (the interactive
    // manager is what bare `/auth` opens).
    if arg == "status" || arg == "show" {
        return auth_status_lines();
    }

    // `<provider> <key>` or `set <provider> <key>`.
    let payload = arg.strip_prefix("set ").map(str::trim).unwrap_or(arg);
    if !payload.is_empty() {
        if let Some((id, key)) = payload.split_once(char::is_whitespace) {
            let id = id.trim();
            let key = key.trim();
            if !key.is_empty() {
                return match store::provider_info(id) {
                    Some(info) => {
                        match store::env_file().map_err(|e| e.to_string()).and_then(|p| {
                            store::set_key_in_env_file(&p, info.env_var, key)
                                .map_err(|e| e.to_string())
                        }) {
                            Ok(()) => {
                                std::env::set_var(info.env_var, key);
                                vec![format!(
                                    "✓ {} key saved ({}) — ready; takes effect on your next launch if already running",
                                    info.name,
                                    store::mask_key(key)
                                )]
                            }
                            Err(e) => vec![format!("cannot save key: {e}")],
                        }
                    }
                    None => vec![unknown_provider_line(id)],
                };
            }
        }
        return vec![auth_usage()];
    }

    // Empty/unrecognized arg (defensive; bare /auth opens the manager).
    vec![auth_usage()]
}

/// The text status board (masked keys + sign-in markers).
fn auth_status_lines() -> Vec<String> {
    use modron_core::auth as store;
    let mut out = vec!["provider credentials:".to_string()];
    for info in store::provider_registry() {
        let (source, value) = store::key_source(info.env_var);
        let mut shown = match source {
            store::KeySource::EnvFile => format!("{} (saved)", store::mask_key(&value)),
            store::KeySource::ProcessEnv => format!("{} (process env)", store::mask_key(&value)),
            store::KeySource::None => String::new(),
        };
        if store::oauth_signed_in(info.provider_id) {
            let marker = if shown.is_empty() {
                String::new()
            } else {
                " · ".to_string()
            };
            shown = format!("{shown}{marker}oauth signed in");
        }
        if shown.is_empty() {
            shown = "—".to_string();
        }
        out.push(format!("  {:<18} {}", info.name, shown));
    }
    out.push(
        "manage in-app: /auth · save: /auth <provider> <key> · remove: /auth remove <provider>"
            .to_string(),
    );
    out
}

fn unknown_provider_line(id: &str) -> String {
    let known: Vec<&str> = modron_core::auth::provider_registry()
        .iter()
        .map(|p| p.provider_id)
        .collect();
    format!("unknown provider `{id}` — known: {}", known.join(", "))
}

fn auth_usage() -> String {
    "/auth — status · /auth <provider> <key> — save · /auth remove <provider>".to_string()
}

enum DeltaOrTool {
    Delta(StreamDelta),
    Tool(ToolActivity),
}

// ---------------------------------------------------------------------------
// Remote-control driver (Phase 3): frames -> loop channels
// ---------------------------------------------------------------------------

/// A protocol frame translated into loop-channel inputs (pure, tested).
enum LoopInput {
    Delta(StreamDelta),
    Tool(ToolActivity),
    Approval { request_id: String, call: ToolCall },
    Done(WireRunSummary),
    Failed(String),
    Aborted,
    Note(String),
    ConfigChanged(ConfigPatch),
    Snapshot(Box<crate::remote::AttachSnapshot>),
}

fn frame_to_inputs(frame: Frame) -> Vec<LoopInput> {
    match frame.payload {
        Payload::Snapshot {
            session_id,
            last_seq,
            events,
            live,
        } => vec![LoopInput::Snapshot(Box::new(
            crate::remote::AttachSnapshot {
                session_id,
                last_seq,
                events,
                live,
            },
        ))],
        Payload::Delta { delta } => vec![LoopInput::Delta(delta.to_core())],
        Payload::Tool { activity } => vec![LoopInput::Tool(activity.into())],
        Payload::ApprovalRequested { request_id, call } => {
            vec![LoopInput::Approval { request_id, call }]
        }
        Payload::TurnDone { summary } => vec![LoopInput::Done(summary)],
        Payload::TurnFailed { error } => vec![LoopInput::Failed(error)],
        Payload::TurnAborted { .. } => vec![LoopInput::Aborted],
        Payload::Note { text } => vec![LoopInput::Note(text)],
        Payload::ConfigChanged { patch } => vec![LoopInput::ConfigChanged(patch)],
        Payload::OAuthProgress {
            provider, message, ..
        } => vec![LoopInput::Note(format!("[{provider}] {message}"))],
        Payload::RemoteControlStarted { url } => vec![LoopInput::Note(format!(
            "remote-control live — open {url} on any device to watch and drive this session"
        ))],
        Payload::Error { message } => vec![LoopInput::Note(format!("server error: {message}"))],
        // Welcome (handshake already done), Event (persisted frames are
        // folded into snapshots; live visibility rides deltas — re-rendering
        // both would duplicate every message), Sessions (server-side picker
        // is a later tranche), Pong.
        _ => Vec::new(),
    }
}

/// A stand-in Engine for remote mode: never called (submits, interrupts
/// and approvals ride the protocol), but the loop's local branches hold
/// the engine by value, so an inert one keeps the code uniform.
fn inert_engine() -> Engine {
    Engine {
        selector: Arc::new(modron_core::testing::StaticSelector::new(
            "unused", "unused",
        )),
        providers: std::collections::HashMap::new(),
        tools: Arc::new(modron_core::ToolRegistry::new()),
        store: Arc::new(modron_store::JsonlSessionStore::new(
            std::env::temp_dir().join("modron-tui-remote"),
        )),
        config: modron_core::EngineConfig::default(),
    }
}

/// Owns the client; forwards frames into the loop's channels and loop
/// commands into the wire. The approval waiter parks a blocking thread
/// (spawn_blocking) — the same park-and-decide shape as the local
/// approver, just across the wire.
fn spawn_remote_driver(
    mut client: crate::remote::RemoteClient,
    mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<LoopCmd>,
    cmd_tx: tokio::sync::mpsc::UnboundedSender<LoopCmd>,
    delta_tx: mpsc::Sender<DeltaOrTool>,
    done_tx: tokio::sync::mpsc::UnboundedSender<Outcome>,
    ui_tx: tokio::sync::mpsc::UnboundedSender<UiFrame>,
    approval: ApprovalBus,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // The adopted session id: set by the auto-attach snapshot (first
        // submit) or by --resume; used for session-scoped commands.
        let mut driver_session: Option<uuid::Uuid> = None;
        loop {
            tokio::select! {
                cmd = cmd_rx.recv() => match cmd {
                    None => break,
                    Some(LoopCmd::Submit { session, task, attachments }) => {
                        let _ = client.send(Payload::Submit {
                            session_id: session.or(driver_session),
                            task,
                            attachments,
                        });
                    }
                    Some(LoopCmd::Interrupt { session }) => {
                        if let Some(session_id) = session.or(driver_session) {
                            let _ = client.send(Payload::Interrupt { session_id });
                        }
                    }
                    Some(LoopCmd::Approve { request_id, decision }) => {
                        let _ = client.send(Payload::Approve { request_id, decision });
                    }
                    Some(LoopCmd::SetConfig(patch)) => {
                        let _ = client.send(Payload::SetConfig { patch });
                    }
                    Some(LoopCmd::RemoteControl { bridge_url }) => {
                        let Some(session_id) = driver_session else {
                            break;
                        };
                        let _ = client.send(Payload::RemoteControl {
                            session_id,
                            bridge_url,
                        });
                    }
                },
                frame = client.next_frame() => match frame {
                    None => {
                        let _ = ui_tx.send(UiFrame::Disconnected);
                        break;
                    }
                    Some(frame) => {
                        for input in frame_to_inputs(frame) {
                            match input {
                                LoopInput::Delta(d) => {
                                    let _ = delta_tx.send(DeltaOrTool::Delta(d));
                                }
                                LoopInput::Tool(t) => {
                                    let _ = delta_tx.send(DeltaOrTool::Tool(t));
                                }
                                LoopInput::Done(summary) => {
                                    let _ = done_tx.send(Outcome::Done(summary.into()));
                                }
                                LoopInput::Failed(error) => {
                                    let _ = done_tx.send(Outcome::Failed(error));
                                }
                                LoopInput::Aborted => {
                                    let _ = done_tx.send(Outcome::Aborted);
                                }
                                LoopInput::Note(text) => {
                                    let _ = ui_tx.send(UiFrame::Note(text));
                                }
                                LoopInput::ConfigChanged(patch) => {
                                    let _ = ui_tx.send(UiFrame::ConfigChanged(patch));
                                }
                                LoopInput::Snapshot(snap) => {
                                    driver_session = Some(snap.session_id);
                                    let _ = ui_tx.send(UiFrame::Snapshot(snap));
                                }
                                LoopInput::Approval { request_id, call } => {
                                    let (respond, receiver) = mpsc::channel();
                                    *approval.slot.lock().unwrap() = Some((call, respond));
                                    let cmd_tx = cmd_tx.clone();
                                    let rest = approval.rest_approved.clone();
                                    tokio::task::spawn_blocking(move || {
                                        let allow = receiver.recv().unwrap_or(false);
                                        let decision = if allow {
                                            if rest.load(Ordering::Relaxed) {
                                                Decision::AllowAll
                                            } else {
                                                Decision::AllowOnce
                                            }
                                        } else {
                                            Decision::Deny
                                        };
                                        let _ = cmd_tx.send(LoopCmd::Approve { request_id, decision });
                                    });
                                }
                            }
                        }
                    }
                },
            }
        }
    })
}

/// Translate a parked slash command into a server SetConfig patch while
/// connected. `None` = no remote equivalent (the loop refuses it with an
/// explicit toast). The optional label mirrors the local /mode arm's
/// status-strip update.
fn remote_slash_command(cmd: &PendingCommand) -> Option<(ConfigPatch, Option<String>)> {
    use modron_protocol::ApprovalModeWire;
    let patch = match cmd {
        PendingCommand::Model { name, .. } => ConfigPatch {
            model: Some(name.clone()),
            ..ConfigPatch::default()
        },
        PendingCommand::Mode(m) => {
            let (wire, label) = match m.trim() {
                "yolo" | "auto" => (ApprovalModeWire::ApproveAll, "yolo"),
                "deny" => (ApprovalModeWire::Deny, "deny"),
                _ => (ApprovalModeWire::Ask, "ask"),
            };
            return Some((
                ConfigPatch {
                    approval_mode: Some(wire),
                    ..ConfigPatch::default()
                },
                Some(label.to_string()),
            ));
        }
        PendingCommand::Sandbox(s) => ConfigPatch {
            sandbox: Some(match s.trim() {
                "seatbelt" => modron_core::SandboxPolicy::Seatbelt,
                "bubblewrap" | "bwrap" => modron_core::SandboxPolicy::Bubblewrap,
                _ => modron_core::SandboxPolicy::Off,
            }),
            ..ConfigPatch::default()
        },
        PendingCommand::Agent(name) => ConfigPatch {
            persona: Some(name.clone()),
            ..ConfigPatch::default()
        },
        PendingCommand::Allow(tool) => ConfigPatch {
            allow: Some(vec![tool.clone()]),
            ..ConfigPatch::default()
        },
        PendingCommand::Deny(tool) => ConfigPatch {
            deny: Some(vec![tool.clone()]),
            ..ConfigPatch::default()
        },
        _ => return None,
    };
    Some((patch, None))
}
