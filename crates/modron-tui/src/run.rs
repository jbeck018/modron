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
use modron_protocol::{ConfigPatch, Frame, Payload, WireRunSummary};
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
    /// /fork in remote mode: the driver fills the session id.
    Fork {
        session: Option<SessionId>,
    },
    /// /tasks in remote mode: the server's worktree-task status.
    Tasks {
        session: Option<SessionId>,
    },
    /// /share in remote mode: publish the captured answer through the bridge.
    Publish {
        session: Option<SessionId>,
        title: String,
        content_md: String,
    },
    /// /automations in remote mode: the server's scheduled prompts.
    ListAutomations,
    /// /new in remote mode: a fresh server session (the driver adopts it
    /// and re-attaches, resetting the transcript).
    NewSession {
        title: Option<String>,
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
    /// New transcript entries folded from a live persisted Event frame
    /// (the driver's ReplayFolder; parity with a fresh attach replay).
    Entries(Vec<modron_view::TranscriptEntry>),
    /// The server settled an approval (any client's decision): clear the
    /// matching prompt.
    ApprovalResolved {
        request_id: String,
    },
    Disconnected,
}

/// Poll cadence for crossterm events (and the spinner tick).
const TICK: Duration = Duration::from_millis(50);

/// The ratatui terminal the render loop draws into (crossterm backend over
/// stdout).
type TuiTerminal = ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>;

/// Owns the terminal's raw-mode/alt-screen lifecycle: [`enter`](Self::enter)
/// installs it and restores it on EVERY exit path — a propagated `?` or a
/// panic past the render loop would otherwise leave the user's shell in raw
/// mode on the alternate screen. Idempotent.
struct TerminalGuard {
    kitty: bool,
    done: bool,
}

impl TerminalGuard {
    /// Terminal setup: inline viewport, raw mode, hidden cursor.
    /// Kitty keyboard protocol (kitty/Ghostty/WezTerm/iTerm2 3.5+): lets the
    /// terminal report modifiers on Enter, making shift+enter a real newline
    /// instead of a submit. The query returns quickly on any terminal that
    /// answers device attributes; unsupported ones ignore the push entirely.
    fn enter() -> anyhow::Result<(Self, TuiTerminal)> {
        let kitty_keyboard = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
        let mut stdout = std::io::stdout();
        crossterm::terminal::enable_raw_mode()?;
        // From here on, every exit path must restore the terminal — the
        // guard's Drop covers error returns and panics the inline teardown
        // cannot.
        let guard = TerminalGuard {
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
        // A failure here unwinds through the guard's Drop: the terminal is
        // already in raw mode on the alt screen.
        let terminal = ratatui::Terminal::new(backend)?;
        Ok((guard, terminal))
    }

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

impl Drop for TerminalGuard {
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
    /// Remote mode: the wire request id of the prompt in `slot` (set by the
    /// driver before it fills the slot). Local approvals leave it empty.
    request_id: Arc<Mutex<Option<String>>>,
}

/// The decision a remote approval waiter sends. `None` means the waiter
/// was released without a verdict (the prompt was dropped locally, or the
/// request was already resolved elsewhere): send nothing, because the
/// server rejects a decision for an unknown request id.
fn remote_decision(received: Option<bool>, allow_rest: bool) -> Option<modron_protocol::Decision> {
    use modron_protocol::Decision;
    match received {
        None => None,
        Some(false) => Some(Decision::Deny),
        Some(true) if allow_rest => Some(Decision::AllowAll),
        Some(true) => Some(Decision::AllowOnce),
    }
}

/// Run the TUI. `build` receives the wired sinks and returns the engine plus
/// the session to resume (if `--resume` was given). Requires a TTY on stdout;
/// returns an actionable error otherwise. The closure is fallible so a failed
/// engine build surfaces as an error (terminal restored by the guard) instead
/// of a panic.
///
/// Composition (each step a helper below): remote connect happens before the
/// terminal goes raw; [`TerminalGuard::enter`] owns raw-mode/alt-screen setup
/// and restores on every exit path; [`build_local_engine`] wires the engine's
/// sinks to this loop; [`build_app`] + [`push_welcome_banner`] set up the
/// first frame; [`RunState`] bundles the loop's channels and spawned-task
/// slots so the event loop reads as the tick order — deltas, approvals,
/// keyboard, pending commands, oauth, slow/clipboard, shell, remote frames,
/// approve-rest mirror, outcomes, render.
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
        request_id: Arc::new(Mutex::new(None)),
    };

    // Remote-control connect happens BEFORE the terminal goes raw: a bad
    // target fails fast with a plain error, nothing to restore.
    let is_remote = remote.is_some();
    let remote_target = remote.as_ref().map(|spec| spec.target.clone());
    let connected = match remote {
        Some(spec) => Some(connect_remote(spec).await?),
        None => None,
    };

    // The engine builder gets sinks that talk to THIS loop. Remote mode
    // never calls it — the engine lives on the server — and the loop's
    // local branches hold an inert stand-in so the code stays uniform.
    let (engine, resume_session) = if connected.is_some() {
        (inert_engine(), None)
    } else {
        build_local_engine(build, &delta_tx, &approval)?
    };

    // Terminal setup (raw mode, kitty flags, bracketed paste, alt screen) +
    // the guard that restores it on EVERY exit path from here on.
    let (mut terminal_restore, mut terminal) = TerminalGuard::enter()?;

    let attached = connected.as_ref().and_then(|(_, snap)| snap.as_ref());
    let mut app = build_app(
        is_remote,
        &engine,
        attached,
        resume_session,
        &model_label,
        upgrade_notice,
    );
    push_welcome_banner(&mut app, &model_label);

    // Which session's log the scrollback currently mirrors; a different
    // session's snapshot (e.g. after /new) resets it first. (Resolved inside
    // backfill_remote_history, along with the remote banner + attach replay.)
    let backfill_session =
        backfill_remote_history(&mut app, remote_target.as_deref(), connected.as_ref());

    #[allow(unused_assignments)]
    let mut result: anyhow::Result<()> = Ok(());

    // The loop's machinery — progress channels, spawned-task slots with
    // their generations, and the remote-control driver (spawned here, after
    // the App is ready, exactly where the inline setup did it) — bundled so
    // each phase below is one call.
    let mut state = RunState::new(
        engine,
        is_remote,
        delta_tx,
        delta_rx,
        approval,
        connected,
        backfill_session,
    );

    loop {
        // 1. Drain engine deltas/tool events.
        state.drain_engine_events(&mut app);

        // 2. Drain the approval slot into the App (redraw happens below).
        state.drain_approval_slot(&mut app);

        // 3. Poll keyboard (raw mode). Skip/Quit reproduce the old loop's
        //    inline `continue`/`break` exactly.
        match state.poll_keyboard(&mut app).await? {
            IterationFlow::Skip => continue,
            IterationFlow::Quit => {
                result = Ok(());
                break;
            }
            IterationFlow::Advance => {}
        }

        // 3b. Slash commands needing engine/store powers.
        state.dispatch_pending_commands(&mut app).await;

        // 3c. OAuth progress.
        state.drain_oauth_progress(&mut app);

        // 3d. Slow-command and clipboard results.
        state.drain_slow_and_clipboard(&mut app);

        // 3e. `!` shell passthrough result.
        state.drain_shell_results(&mut app);

        // 3f. Remote-control updates.
        state.drain_ui_frames(&mut app);

        // 4. Approve-rest mirror.
        state.mirror_approve_rest(&mut app);

        // 5. Engine settled?
        state.drain_outcomes(&mut app);

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
    state.adopt_pending_fork(&mut app);
    if let Some(id) = app.session_id {
        println!("session: {id} — resume with --resume {id}");
    }
    result
}

// ---------------------------------------------------------------------------
// run_with building blocks: connect, engine wiring, app bootstrap
// ---------------------------------------------------------------------------

/// A successful remote connect: the client plus, when `--resume` named a
/// session, its attach snapshot.
type ConnectedRemote = (
    crate::remote::RemoteClient,
    Option<crate::remote::AttachSnapshot>,
);

/// Connect to the remote-control server and resolve the resume spec into a
/// session: `None` = fresh on first submit; `Some(None)` = the server's
/// newest session (`--resume` bare); `Some(Some(id))` = that id. Attaches the
/// session when resolved (snapshot = persisted transcript + live state).
async fn connect_remote(spec: RemoteSpec) -> anyhow::Result<ConnectedRemote> {
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
    Ok((client, snapshot))
}

/// The local engine's sinks talk to THIS loop: deltas and tool events feed
/// the std channel; the approver queues the call into the shared slot and
/// parks the engine task until the render loop delivers the decision.
fn build_local_engine(
    build: impl FnOnce(TuiSinks) -> anyhow::Result<(Engine, Option<SessionId>)>,
    delta_tx: &mpsc::Sender<DeltaOrTool>,
    approval: &ApprovalBus,
) -> anyhow::Result<(Engine, Option<SessionId>)> {
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
    })
}

/// Build the App's first frame. Remote attach state drives a few App fields
/// the local path takes from the engine (menus degrade gracefully: no
/// personas/tool lists remotely yet — the server does not expose them over
/// the protocol).
fn build_app(
    is_remote: bool,
    engine: &Engine,
    attached: Option<&crate::remote::AttachSnapshot>,
    resume_session: Option<SessionId>,
    model_label: &str,
    upgrade_notice: Option<String>,
) -> App {
    App {
        upgrade_notice,
        session_id: attached.map(|snap| snap.session_id).or(resume_session),
        model_label: attached
            .and_then(|snap| (!snap.live.model.is_empty()).then(|| snap.live.model.clone()))
            .unwrap_or_else(|| model_label.to_string()),
        context_window: modron_router::lookup(model_label)
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
    }
}

/// Welcome banner (Phase 6g): the TUI opens onto content, not a void —
/// now the first lines of the internal scrollback.
fn push_welcome_banner(app: &mut App, model_label: &str) {
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
            Span::styled(model_label.to_owned(), Style::default()),
            Span::styled(
                "  ·  enter send · shift/alt+enter newline · pgup/pgdn scroll · esc interrupt/live",
                Style::default().fg(crate::DIM),
            ),
        ]),
        Line::from(""),
    ];
    app.push_history(welcome);
}

/// Remote banner note + attach replay: the persisted transcript lands
/// in scrollback (same styles as the live path) before anything runs.
/// Returns which session's log the scrollback now mirrors (a different
/// session's snapshot — e.g. after /new — resets it first; the loop keeps
/// tracking this in [`RunState::backfill_session`]).
fn backfill_remote_history(
    app: &mut App,
    remote_target: Option<&str>,
    connected: Option<&ConnectedRemote>,
) -> Option<SessionId> {
    if let Some(target) = remote_target {
        app.note(format!(
            "connected to {target} — sessions live on the server and survive this TUI"
        ));
    }
    let mut backfill_session: Option<SessionId> = None;
    if let Some((_, Some(snap))) = connected {
        backfill_session = Some(snap.session_id);
        if !snap.events.is_empty() {
            app.reset_transcript();
            app.backfill_transcript(modron_view::replay(&snap.events));
        }
    }
    backfill_session
}

/// What the loop does after the keyboard phase — the extracted phase's
/// explicit form of the old inline `continue`/`break`, so iteration flow is
/// preserved exactly: `Skip` leaves the remaining phases of this tick (and
/// the redraw) untouched, `Quit` leaves the loop.
enum IterationFlow {
    /// Fall through to the remaining drains and the render.
    Advance,
    /// The old loop's `continue` (paste, busy submit, remote submit).
    Skip,
    /// The old loop's `break` on `Action::Quit`.
    Quit,
}

/// The run loop's machinery, bundled so each phase of an iteration is one
/// method call: the engine (local mode; an inert stand-in remotely), the
/// progress channels, and the spawned-task slots with their generations.
struct RunState {
    engine: Engine,
    is_remote: bool,
    approval: ApprovalBus,
    // Loop <- producers (drained each tick, in phase order).
    delta_rx: mpsc::Receiver<DeltaOrTool>,
    done_rx: tokio::sync::mpsc::UnboundedReceiver<Outcome>,
    oauth_rx: tokio::sync::mpsc::UnboundedReceiver<OAuthProgress>,
    slow_rx: tokio::sync::mpsc::UnboundedReceiver<SlowCommandProgress>,
    clip_rx: tokio::sync::mpsc::UnboundedReceiver<ClipboardProgress>,
    shell_rx: tokio::sync::mpsc::UnboundedReceiver<ShellOutcome>,
    ui_rx: tokio::sync::mpsc::UnboundedReceiver<UiFrame>,
    // Loop -> producers (cloned into the spawned tasks / the driver).
    done_tx: tokio::sync::mpsc::UnboundedSender<Outcome>,
    oauth_tx: tokio::sync::mpsc::UnboundedSender<OAuthProgress>,
    slow_tx: tokio::sync::mpsc::UnboundedSender<SlowCommandProgress>,
    clip_tx: tokio::sync::mpsc::UnboundedSender<ClipboardProgress>,
    shell_tx: tokio::sync::mpsc::UnboundedSender<ShellOutcome>,
    loop_cmd_tx: tokio::sync::mpsc::UnboundedSender<LoopCmd>,
    // Spawned-task slots: at most one of each is in flight; Esc and quit
    // abort by handle.
    engine_task: Option<tokio::task::JoinHandle<()>>,
    // Remote mode's one-at-a-time flag (no JoinHandle exists to check).
    remote_busy: bool,
    // Remote mode: the wire request id of the prompt the App is showing
    // (matched against ApprovalResolved so a decision made elsewhere
    // clears it).
    approval_request: Option<String>,
    oauth_task: Option<tokio::task::JoinHandle<()>>,
    oauth_generation: u64,
    slow_command_task: Option<tokio::task::JoinHandle<()>>,
    slow_command_generation: u64,
    harvest_task: Option<tokio::task::JoinHandle<()>>,
    shell_task: Option<tokio::task::JoinHandle<()>>,
    // The first snapshot with events replays the transcript; later ones
    // (auto-attach echoes) must not duplicate history.
    // Which session's log the scrollback currently mirrors; a different
    // session's snapshot (e.g. after /new) resets it first.
    backfill_session: Option<SessionId>,
    // Remote mode: this connection's copy of the allow/deny lists. The
    // server replaces a list on SetConfig, so /allow and /deny send the
    // whole list from here.
    remote_lists: RemoteLists,
}

/// Client-side mirror of the server's allow and deny lists for one
/// connection (starts empty; C5 will carry the real lists in the snapshot).
#[derive(Default)]
struct RemoteLists {
    allow: Vec<String>,
    deny: Vec<String>,
}

/// What a parked slash command becomes while connected.
enum RemoteSlash {
    /// Send this SetConfig patch; the optional label updates the status strip.
    Patch(ConfigPatch, Option<String>),
    /// Nothing is sent; show this note to the user.
    Note(String),
    /// No remote equivalent: the command needs the machine that owns the engine.
    Unsupported,
}

impl RunState {
    /// Create the progress channels, bundle the task slots, and (when
    /// connected) spawn the remote-control driver.
    fn new(
        engine: Engine,
        is_remote: bool,
        delta_tx: mpsc::Sender<DeltaOrTool>,
        delta_rx: mpsc::Receiver<DeltaOrTool>,
        approval: ApprovalBus,
        connected: Option<ConnectedRemote>,
        backfill_session: Option<SessionId>,
    ) -> Self {
        let (done_tx, done_rx) = tokio::sync::mpsc::unbounded_channel::<Outcome>();
        // OAuth progress from the spawned flows: the interim update shows the
        // URL (+ device-flow user code) in the modal, Done settles it.
        let (oauth_tx, oauth_rx) = tokio::sync::mpsc::unbounded_channel::<OAuthProgress>();
        // Slow slash commands (/tasks, /fork) run OFF the render loop — same
        // spawn + channel + generation pattern as the OAuth flows — so redraws
        // and Esc keep working while they await tools/store I/O.
        let (slow_tx, slow_rx) = tokio::sync::mpsc::unbounded_channel::<SlowCommandProgress>();
        // Clipboard I/O (pbcopy/wl-copy/osascript subprocesses) is also async.
        let (clip_tx, clip_rx) = tokio::sync::mpsc::unbounded_channel::<ClipboardProgress>();
        // `!` shell passthrough is async too: a slow `make` must never freeze
        // the render loop, and Esc gets to kill it (kill_on_drop).
        let (shell_tx, shell_rx) = tokio::sync::mpsc::unbounded_channel::<ShellOutcome>();
        // Remote-control driver: owns the client, translates frames into the
        // loop's channels. The loop talks to it through loop_cmd_tx.
        let (loop_cmd_tx, loop_cmd_rx) = tokio::sync::mpsc::unbounded_channel::<LoopCmd>();
        let (ui_tx, ui_rx) = tokio::sync::mpsc::unbounded_channel::<UiFrame>();
        if let Some((client, _)) = connected {
            spawn_remote_driver(
                client,
                loop_cmd_rx,
                loop_cmd_tx.clone(),
                delta_tx.clone(),
                done_tx.clone(),
                ui_tx,
                approval.clone(),
            );
        }
        Self {
            engine,
            is_remote,
            approval,
            delta_rx,
            done_rx,
            oauth_rx,
            slow_rx,
            clip_rx,
            shell_rx,
            ui_rx,
            done_tx,
            oauth_tx,
            slow_tx,
            clip_tx,
            shell_tx,
            loop_cmd_tx,
            engine_task: None,
            remote_busy: false,
            approval_request: None,
            oauth_task: None,
            oauth_generation: 0,
            slow_command_task: None,
            slow_command_generation: 0,
            harvest_task: None,
            shell_task: None,
            backfill_session,
            remote_lists: RemoteLists::default(),
        }
    }

    /// Phase 1 of each tick: drain engine deltas/tool events. Called again
    /// from [`RunState::drain_outcomes`] so deltas queued behind a Done
    /// still land before the turn settles.
    fn drain_engine_events(&mut self, app: &mut App) {
        while let Ok(msg) = self.delta_rx.try_recv() {
            match msg {
                DeltaOrTool::Delta(d) => app.on_delta(d),
                DeltaOrTool::Tool(t) => app.on_tool(t),
            }
        }
    }

    /// Phase 2: drain the approval slot into the App (redraw happens in the
    /// render phase). The slot carries the REAL ToolCall (remote approvals
    /// arrive with one; the local approver clones the one it was given) —
    /// the old summary-string reconstruction is gone.
    fn drain_approval_slot(&mut self, app: &mut App) {
        if app.pending_approval.is_none() {
            if let Some((call, respond)) = self.approval.slot.lock().unwrap().take() {
                self.approval_request = self.approval.request_id.lock().unwrap().take();
                app.pending_approval = Some(PendingApproval { call, respond });
            }
        }
    }

    /// D6 run boundary: "allow all" and the pending prompt (App and the
    /// approver bus, including a queued slot entry) are cleared together.
    /// Called when a run starts, settles, is aborted locally, or when the
    /// connection drops or /mode and /new change the session's approvals.
    fn reset_run_approvals(&mut self, app: &mut App) {
        app.clear_run_approvals();
        self.approval
            .rest_approved
            .store(false, std::sync::atomic::Ordering::Relaxed);
        // Dropping a queued (slot) or shown respond releases the engine
        // with no decision (local: denied; remote: nothing is sent).
        self.approval.slot.lock().unwrap().take();
        self.approval.request_id.lock().unwrap().take();
        self.approval_request = None;
    }

    /// Phase 3: poll the keyboard (raw mode). Returns [`IterationFlow`] so
    /// this extracted phase reproduces the old inline `continue`/`break`
    /// verbs exactly — a paste, for instance, must not force a redraw this
    /// tick.
    async fn poll_keyboard(&mut self, app: &mut App) -> anyhow::Result<IterationFlow> {
        if !crossterm::event::poll(TICK)? {
            return Ok(IterationFlow::Advance);
        }
        match crossterm::event::read()? {
            crossterm::event::Event::Paste(text) => {
                // Bracketed paste: multi-line text enters the input
                // verbatim — the terminal told us it is a paste, so it
                // must never submit. The auth wizard's key entry takes
                // precedence when open.
                let text = text.replace("\r\n", "\n").replace('\r', "\n");
                app.paste_into_auth_or_input(&text);
                return Ok(IterationFlow::Skip);
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
                        let tx = self.clip_tx.clone();
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
                        if self.harvest_task.is_some() {
                            app.toast("clipboard read already in flight");
                            return Ok(IterationFlow::Skip);
                        }
                        app.toast("reading clipboard…");
                        let tx = self.clip_tx.clone();
                        self.harvest_task = Some(tokio::spawn(async move {
                            let content = tokio::task::spawn_blocking(crate::clipboard::harvest)
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
                                return Ok(IterationFlow::Skip);
                            }
                            if self.shell_task.is_some() {
                                app.toast("a shell command is still running — Esc cancels it");
                                return Ok(IterationFlow::Skip);
                            }
                            let accent =
                                ratatui::style::Style::default().fg(app.theme.resolved().accent);
                            app.push_history(vec![ratatui::text::Line::from(format!(
                                "❯ !{command}"
                            ))
                            .style(accent)]);
                            let tx = self.shell_tx.clone();
                            let dir = if self.is_remote {
                                // `!` runs on the client machine; the
                                // server's working dir is its own.
                                std::env::current_dir()
                                    .unwrap_or_else(|_| std::path::PathBuf::from("."))
                            } else {
                                self.engine.config.working_dir.clone()
                            };
                            self.shell_task = Some(tokio::spawn(async move {
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
                            return Ok(IterationFlow::Skip);
                        }
                        // One engine task at a time: a second concurrent
                        // spawn would overwrite the first JoinHandle
                        // (unabortable orphan streaming into the App,
                        // double finish_turn, clobbered approval slot).
                        if self.engine_task.is_some() || self.remote_busy {
                            app.toast("a turn is already running — Esc aborts it");
                            return Ok(IterationFlow::Skip);
                        }
                        // D6: a new run starts with no allow-all and no
                        // prompt carried over from the last one.
                        self.reset_run_approvals(app);
                        let task = app.task_with_goal(&task);
                        app.begin_task(&task);
                        let attachments = app.take_attachments();
                        if self.is_remote {
                            // Server-authoritative: the session id
                            // arrives on the auto-attach Snapshot
                            // (first submit) or is already known;
                            // TurnDone/Aborted clear the busy flag.
                            let _ = self.loop_cmd_tx.send(LoopCmd::Submit {
                                session: app.session_id,
                                task,
                                attachments,
                            });
                            self.remote_busy = true;
                            return Ok(IterationFlow::Skip);
                        }
                        let engine = self.engine.clone();
                        let done = self.done_tx.clone();
                        let session = app.session_id;
                        self.engine_task = Some(tokio::spawn(async move {
                            let outcome = match session {
                                Some(id) => {
                                    engine
                                        .run_task_resuming_with_attachments(id, &task, attachments)
                                        .await
                                }
                                None => engine.run_task_with_attachments(&task, attachments).await,
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
                        if self.is_remote {
                            // Server-authoritative interrupt: TurnAborted
                            // drives finish_interrupted (a dead server
                            // leaves the turn running rather than a fake
                            // local abort).
                            if self.remote_busy {
                                let _ = self.loop_cmd_tx.send(LoopCmd::Interrupt {
                                    session: app.session_id,
                                });
                            }
                        } else {
                            match (self.engine_task.take(), self.slow_command_task.take()) {
                                (Some(task), _) => {
                                    task.abort();
                                    // Give the runtime a beat to cancel.
                                    tokio::time::sleep(Duration::from_millis(30)).await;
                                    app.finish_interrupted();
                                    self.reset_run_approvals(app);
                                }
                                // No engine running: Esc cancels a slow
                                // command instead. Bumping the generation
                                // drops any Done already sitting in the
                                // channel (an aborted /fork must not adopt
                                // its new session id).
                                (None, Some(task)) => {
                                    self.slow_command_generation += 1;
                                    task.abort();
                                    app.toast("command cancelled");
                                }
                                (None, None) => {}
                            }
                        }
                        // A pending `!` shell job cancels with Esc too
                        // (kill_on_drop kills the child process).
                        if let Some(task) = self.shell_task.take() {
                            task.abort();
                            app.toast("shell command cancelled");
                        }
                    }
                    crate::Action::Quit => {
                        if let Some(task) = self.engine_task.take() {
                            task.abort();
                        }
                        if let Some(task) = self.slow_command_task.take() {
                            self.slow_command_generation += 1;
                            task.abort();
                        }
                        return Ok(IterationFlow::Quit);
                    }
                }
            }
            _ => {}
        }
        Ok(IterationFlow::Advance)
    }

    /// Phase 3b: slash commands needing engine/store powers (Phase 14) —
    /// the App parks a PendingCommand; execute it here, the result lines
    /// land in the internal scrollback. The OAuth logins are the exception:
    /// they wait on browser round-trips, so they run as spawned tasks (the
    /// TUI keeps rendering; Esc on the modal cancels them).
    async fn dispatch_pending_commands(&mut self, app: &mut App) {
        // NOTE: peek with matches! before take() — an `if let` over
        // `.take()` drops the value when the pattern fails.
        // D6: changing the approval mode or starting a new session ends the
        // current allow-all and any open prompt (local and remote alike).
        if matches!(
            app.pending_command,
            Some(PendingCommand::Mode(_)) | Some(PendingCommand::NewSession)
        ) {
            self.reset_run_approvals(app);
        }
        if self.is_remote {
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
                            let _ = self
                                .loop_cmd_tx
                                .send(LoopCmd::RemoteControl { bridge_url: url });
                        }
                    }
                    PendingCommand::Fork => {
                        let _ = self.loop_cmd_tx.send(LoopCmd::Fork { session: None });
                    }
                    PendingCommand::Tasks => {
                        let _ = self.loop_cmd_tx.send(LoopCmd::Tasks { session: None });
                    }
                    PendingCommand::Publish { title, content_md } => {
                        let _ = self.loop_cmd_tx.send(LoopCmd::Publish {
                            session: None,
                            title,
                            content_md,
                        });
                    }
                    PendingCommand::Automations => {
                        let _ = self.loop_cmd_tx.send(LoopCmd::ListAutomations);
                    }
                    PendingCommand::NewSession => {
                        let _ = self.loop_cmd_tx.send(LoopCmd::NewSession { title: None });
                    }
                    other => match remote_slash_command(&other, &mut self.remote_lists) {
                        RemoteSlash::Patch(patch, mode_label) => {
                            if let Some(label) = mode_label {
                                app.approval_mode = label;
                            }
                            let _ = self.loop_cmd_tx.send(LoopCmd::SetConfig(patch));
                        }
                        RemoteSlash::Note(text) => app.toast(text),
                        RemoteSlash::Unsupported => {
                            app.toast(
                                "needs the machine that owns the engine — run it there                                  (restart without --connect)",
                            );
                        }
                    },
                }
            }
        } else if matches!(app.pending_command, Some(PendingCommand::OAuthLogin { .. })) {
            let Some(PendingCommand::OAuthLogin { session }) = app.pending_command.take() else {
                unreachable!("matches! guard above")
            };
            self.oauth_generation += 1;
            let gen = self.oauth_generation;
            let tx = self.oauth_tx.clone();
            self.oauth_task = Some(tokio::spawn(async move {
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
            self.oauth_generation += 1;
            let gen = self.oauth_generation;
            let tx = self.oauth_tx.clone();
            self.oauth_task = Some(tokio::spawn(async move {
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
            self.oauth_generation += 1;
            let gen = self.oauth_generation;
            let tx = self.oauth_tx.clone();
            self.oauth_task = Some(tokio::spawn(async move {
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
            && self.slow_command_task.is_none()
            && self.engine_task.is_none()
        {
            // Slow commands (/tasks runs a tool, /fork rewrites the whole
            // session log) spawn instead of blocking the render loop.
            let Some(cmd) = app.pending_command.take() else {
                unreachable!("guarded by is_some_and above")
            };
            self.slow_command_generation += 1;
            let gen = self.slow_command_generation;
            let engine_for_task = self.engine.clone();
            let tx = self.slow_tx.clone();
            let session = app.session_id;
            app.toast(format!("/{} running (Esc cancels)…", cmd_label(&cmd)));
            self.slow_command_task = Some(tokio::spawn(async move {
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
            for line in run_pending_command(&mut self.engine, cmd).await {
                app.note(line);
            }
        }
    }

    /// Phase 3c: OAuth progress — interim updates render in the waiting
    /// modal, Done settles it. The spawned flow is aborted whenever the
    /// waiting screen isn't up (modal closed OR backed out to another
    /// manager screen). Messages from an older (cancelled/replaced) flow
    /// generation are dropped — they must not stomp the current modal.
    fn drain_oauth_progress(&mut self, app: &mut App) {
        if !matches!(app.auth_flow, Some(AuthFlow::WaitingOAuth { .. })) {
            if let Some(task) = self.oauth_task.take() {
                task.abort();
            }
        }
        while let Ok(progress) = self.oauth_rx.try_recv() {
            let stale = match &progress {
                OAuthProgress::Interim { gen, .. } | OAuthProgress::Done { gen, .. } => {
                    *gen != self.oauth_generation
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
                    self.oauth_task = None;
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
    }

    /// Phase 3d: slow-command and clipboard results land here (spawned
    /// above); stale generations (aborted/replaced commands) are dropped.
    fn drain_slow_and_clipboard(&mut self, app: &mut App) {
        while let Ok(progress) = self.slow_rx.try_recv() {
            let SlowCommandProgress::Done { gen, result } = progress;
            if gen != self.slow_command_generation {
                continue;
            }
            self.slow_command_task = None;
            for line in result.lines {
                app.note(line);
            }
            if let Some(id) = result.new_session {
                app.session_id = Some(id);
            }
        }
        while let Ok(progress) = self.clip_rx.try_recv() {
            if matches!(progress, ClipboardProgress::Harvested(_)) {
                self.harvest_task = None;
            }
            apply_clipboard_progress(app, progress);
        }
    }

    /// Phase 3e: `!` shell passthrough result — stdout, stderr and the exit
    /// code land as dim lines under the accent echo, bounded like the bash
    /// tool's capture — a runaway `cat` must not flood the scrollback.
    fn drain_shell_results(&mut self, app: &mut App) {
        while let Ok(outcome) = self.shell_rx.try_recv() {
            self.shell_task = None;
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
    }

    /// Phase 3f: remote-control updates — notes (server toasts/slash
    /// output, oauth progress), config echoes (mode/model labels), and
    /// auto-attach snapshots (the session id arrives here on a first
    /// submit to a fresh session).
    fn drain_ui_frames(&mut self, app: &mut App) {
        while let Ok(frame) = self.ui_rx.try_recv() {
            match frame {
                UiFrame::Note(text) => app.note(text),
                UiFrame::ConfigChanged(patch) => {
                    if let Some(mode) = patch.approval_mode {
                        app.approval_mode = approval_label(mode).into();
                    }
                    if let Some(model) = patch.model {
                        app.model_label = model.clone();
                        app.current_model = model;
                    }
                }
                UiFrame::Snapshot(snap) => {
                    app.session_id = Some(snap.session_id);
                    // C5: the server's real mode label, and this
                    // connection's allow/deny copy seeded from the server
                    // (a later /allow or /deny sends the full list).
                    if let Some(mode) = snap.live.approval_mode {
                        app.approval_mode = approval_label(mode).into();
                    }
                    self.remote_lists = RemoteLists {
                        allow: snap.live.allow.clone(),
                        deny: snap.live.deny.clone(),
                    };
                    if self.backfill_session != Some(snap.session_id) {
                        self.backfill_session = Some(snap.session_id);
                        app.reset_transcript();
                    }
                    if !snap.events.is_empty() {
                        app.backfill_transcript(modron_view::replay(&snap.events));
                    }
                    if !snap.live.model.is_empty() && app.current_model != snap.live.model {
                        app.model_label = snap.live.model.clone();
                        app.current_model = snap.live.model.clone();
                    }
                }
                UiFrame::Entries(entries) => {
                    app.backfill_transcript(entries);
                }
                UiFrame::ApprovalResolved { request_id } => {
                    if self.approval_request.as_deref() == Some(request_id.as_str()) {
                        self.approval_request = None;
                        app.pending_approval = None;
                    }
                    // Not drained yet: drop the queued prompt for it too.
                    let mut queued_id = self.approval.request_id.lock().unwrap();
                    if queued_id.as_deref() == Some(request_id.as_str()) {
                        *queued_id = None;
                        self.approval.slot.lock().unwrap().take();
                    }
                }
                UiFrame::Disconnected => {
                    self.remote_busy = false;
                    self.reset_run_approvals(app);
                    app.toast("server disconnected — restart with --connect to reattach");
                }
            }
        }
    }

    /// Phase 4: approve-rest mirror — when the user pressed `a`, future
    /// approvals short-circuit in the approver; drain any queued one so an
    /// approval parked BEFORE the `a` press is answered, not re-prompted.
    fn mirror_approve_rest(&mut self, app: &mut App) {
        // Mirror both ways: the App is the source of truth for the run's
        // allow-all, and a cleared App flag must clear the approver's too.
        self.approval
            .rest_approved
            .store(app.approve_rest, std::sync::atomic::Ordering::Relaxed);
        if app.approve_rest {
            if let Some((_, respond)) = self.approval.slot.lock().unwrap().take() {
                self.approval.request_id.lock().unwrap().take();
                let _ = respond.send(true);
            }
        }
    }

    /// Phase 5: engine settled? Re-drain the event channels first — deltas
    /// and tool activities can queue behind the Done notification.
    fn drain_outcomes(&mut self, app: &mut App) {
        while let Ok(outcome) = self.done_rx.try_recv() {
            self.engine_task = None;
            self.remote_busy = false;
            self.drain_engine_events(app);
            // The run settled: its allow-all and any open prompt end with it.
            self.reset_run_approvals(app);
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
    }

    /// Post-loop drain: a /fork that landed during the final tick still
    /// adopts its session.
    fn adopt_pending_fork(&mut self, app: &mut App) {
        while let Ok(SlowCommandProgress::Done { gen, result }) = self.slow_rx.try_recv() {
            if gen == self.slow_command_generation {
                if let Some(id) = result.new_session {
                    app.session_id = Some(id);
                }
            }
        }
    }
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
        PendingCommand::RemoteControl { .. } | PendingCommand::Automations => {
            vec!["needs a server session - run `modron serve`, connect with \
             --connect, then /remote-control <bridge url>"
                .to_string()]
        }
        // The content rides the wire from the connected session; without a
        // connection there is nothing to publish.
        PendingCommand::Publish { .. } => {
            vec![
                "publishing runs on the server - run `modron serve`, connect with \
             --connect (the server also needs --bridge <url>)"
                    .to_string(),
            ]
        }
        // Local TUI IS the session: a fresh one means a fresh process (or
        // /fork to branch this one).
        PendingCommand::NewSession => {
            vec![
                "the TUI is the session here - quit and relaunch for a fresh \
             one, or /fork to branch this conversation"
                    .to_string(),
            ]
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
        PendingCommand::Sandbox(mode) => match parse_sandbox_policy(&mode) {
            Some(policy) => {
                engine.config.sandbox = policy;
                vec![match policy {
                    modron_core::SandboxPolicy::Off => "sandbox: off (approval gates only)".into(),
                    modron_core::SandboxPolicy::Seatbelt => {
                        "sandbox: seatbelt — applies to future tool calls".into()
                    }
                    modron_core::SandboxPolicy::Bubblewrap => {
                        "sandbox: bubblewrap — applies to future tool calls".into()
                    }
                }]
            }
            None => vec![format!(
                "usage: /sandbox off|seatbelt|bubblewrap (got {mode:?})"
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
                session_id: session,
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
    ApprovalResolved { request_id: String },
}

fn frame_to_inputs(frame: Frame) -> Vec<LoopInput> {
    match frame.payload {
        Payload::Snapshot {
            session_id,
            last_seq,
            events,
            live,
        } => {
            // A snapshot taken while an approval awaits a decision adopts
            // that prompt (same path as a live ApprovalRequested).
            let adopt = live.pending_approval.clone();
            let mut inputs = vec![LoopInput::Snapshot(Box::new(
                crate::remote::AttachSnapshot {
                    session_id,
                    last_seq,
                    events,
                    live,
                },
            ))];
            if let Some(p) = adopt {
                inputs.push(LoopInput::Approval {
                    request_id: p.request_id,
                    call: p.call,
                });
            }
            inputs
        }
        Payload::ApprovalResolved { request_id, .. } => {
            vec![LoopInput::ApprovalResolved { request_id }]
        }
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
        Payload::Error { message, .. } => vec![LoopInput::Note(format!("server error: {message}"))],
        // Worktree-task status as a compact text block (the desktop shows
        // a review overlay; the TUI reads).
        Payload::TaskStatus {
            worktree,
            branch,
            running,
            ahead,
            files,
            ..
        } => {
            let mut lines = vec![format!(
                "task {branch}: {ahead} ahead{} · {worktree}",
                if running { " · running" } else { "" }
            )];
            if files.is_empty() {
                lines.push("  (no changes vs base)".to_string());
            } else {
                for f in &files {
                    lines.push(format!("  {} (+{} -{})", f.path, f.added, f.deleted));
                }
            }
            lines.into_iter().map(LoopInput::Note).collect()
        }
        Payload::PagePublished { url } => vec![LoopInput::Note(format!(
            "published: {url} - open on any device"
        ))],
        Payload::Automations { rows } => {
            if rows.is_empty() {
                vec![LoopInput::Note("no automations configured".into())]
            } else {
                rows.iter()
                    .map(|a| {
                        LoopInput::Note(format!(
                            "  {} · every {}m · {}{}",
                            a.name,
                            a.interval_minutes,
                            a.prompt,
                            match &a.last_run {
                                None => String::new(),
                                Some(run) if run.is_empty() => String::new(),
                                Some(run) => format!(" (last run {run})"),
                            }
                        ))
                    })
                    .chain(std::iter::once(LoopInput::Note(format!(
                        "{} automation(s)",
                        rows.len()
                    ))))
                    .collect()
            }
        }
        // Welcome (handshake already done), Event (the driver folds it
        // into the running transcript), Sessions (server-side picker is a
        // later tranche), Pong.
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
        // Live fold of persisted events (2026-11-12 audit fix): plan /
        // model / tool-summary events now render live instead of only on
        // the next attach; the folder matches a fresh replay exactly.
        let mut folder = modron_view::ReplayFolder::new();
        let mut folder_session: Option<uuid::Uuid> = None;
        // The last approval request queued for the loop (dedupes a prompt
        // that arrives both live and in an attach snapshot).
        let mut seen_approval: Option<String> = None;
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
                        match remote_control_payload(driver_session, bridge_url) {
                            Ok(payload) => {
                                let _ = client.send(payload);
                            }
                            Err(note) => {
                                let _ = ui_tx.send(UiFrame::Note(note));
                            }
                        }
                    }
                    Some(LoopCmd::Fork { session }) => {
                        if let Some(session_id) = session.or(driver_session) {
                            let _ = client.send(Payload::Fork { session_id });
                        } else {
                            let _ = ui_tx.send(UiFrame::Note(
                                "no session yet - send a task first, then /fork".into(),
                            ));
                        }
                    }
                    Some(LoopCmd::Tasks { session }) => {
                        if let Some(session_id) = session.or(driver_session) {
                            let _ = client.send(Payload::GetTaskStatus { session_id });
                        } else {
                            let _ = ui_tx.send(UiFrame::Note(
                                "no session yet - send a task first, then /tasks".into(),
                            ));
                        }
                    }
                    Some(LoopCmd::Publish {
                        session,
                        title,
                        content_md,
                    }) => {
                        if let Some(session_id) = session.or(driver_session) {
                            let _ = client.send(Payload::PublishPage {
                                session_id,
                                title,
                                content_md,
                            });
                        } else {
                            let _ = ui_tx.send(UiFrame::Note(
                                "no session yet - send a task first, then /share".into(),
                            ));
                        }
                    }
                    Some(LoopCmd::ListAutomations) => {
                        let _ = client.send(Payload::AutomationsList {});
                    }
                    Some(LoopCmd::NewSession { title }) => {
                        let _ = client.send(Payload::NewSession { title });
                    }
                },
                frame = client.next_frame() => match frame {
                    None => {
                        let _ = ui_tx.send(UiFrame::Disconnected);
                        break;
                    }
                    Some(frame) => {
                        // /new adoption (stateful): the correlated reply
                        // names the fresh session; attach so its snapshot
                        // and live fold take over.
                        if let (Some(id), Payload::Note { text }) =
                            (frame.session, &frame.payload)
                        {
                            if text.starts_with("session created: ") {
                                driver_session = Some(id);
                                folder_session = Some(id);
                                folder = modron_view::ReplayFolder::new();
                                let _ = client
                                    .send(Payload::Attach { session_id: id, last_seq: 0 });
                            }
                        }
                        // Live persisted event: fold incrementally (parity
                        // with a fresh attach replay) and surface the new
                        // rows immediately.
                        if let Payload::Event { event } = &frame.payload {
                            let new = folder.push(event).to_vec();
                            if !new.is_empty() {
                                let _ = ui_tx.send(UiFrame::Entries(new));
                            }
                        }
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
                                    if folder_session != Some(snap.session_id) {
                                        folder_session = Some(snap.session_id);
                                        folder = modron_view::ReplayFolder::new();
                                        for event in &snap.events {
                                            folder.push(event);
                                        }
                                    }
                                    let _ = ui_tx.send(UiFrame::Snapshot(snap));
                                }
                                LoopInput::ApprovalResolved { request_id } => {
                                    let _ = ui_tx.send(UiFrame::ApprovalResolved { request_id });
                                }
                                LoopInput::Approval { request_id, call } => {
                                    // A live prompt and the snapshot's copy of
                                    // the same request must not queue twice.
                                    if seen_approval.as_deref() == Some(request_id.as_str()) {
                                        continue;
                                    }
                                    seen_approval = Some(request_id.clone());
                                    let (respond, receiver) = mpsc::channel();
                                    *approval.request_id.lock().unwrap() = Some(request_id.clone());
                                    *approval.slot.lock().unwrap() = Some((call, respond));
                                    let cmd_tx = cmd_tx.clone();
                                    let rest = approval.rest_approved.clone();
                                    tokio::task::spawn_blocking(move || {
                                        // Released without a verdict (dropped
                                        // locally, or resolved elsewhere): send
                                        // nothing; the server rejects unknown ids.
                                        let received = receiver.recv().ok();
                                        if let Some(decision) = remote_decision(
                                            received,
                                            rest.load(Ordering::Relaxed),
                                        ) {
                                            let _ = cmd_tx
                                                .send(LoopCmd::Approve { request_id, decision });
                                        }
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

/// The status-strip words for a wire approval mode (ConfigChanged and the
/// attach snapshot both use this).
fn approval_label(mode: modron_protocol::ApprovalModeWire) -> &'static str {
    use modron_protocol::ApprovalModeWire;
    match mode {
        ApprovalModeWire::ApproveAll => "yolo",
        ApprovalModeWire::Deny => "deny",
        ApprovalModeWire::Ask => "ask",
    }
}

/// Translate a parked slash command into a server SetConfig patch while
/// connected. `None` = no remote equivalent (the loop refuses it with an
/// explicit toast). The optional label mirrors the local /mode arm's
/// status-strip update.
fn remote_slash_command(cmd: &PendingCommand, lists: &mut RemoteLists) -> RemoteSlash {
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
            return RemoteSlash::Patch(
                ConfigPatch {
                    approval_mode: Some(wire),
                    ..ConfigPatch::default()
                },
                Some(label.to_string()),
            );
        }
        // An unknown value must never reach the server: the old mapping
        // turned any typo into Off.
        PendingCommand::Sandbox(s) => match parse_sandbox_policy(s) {
            Some(policy) => ConfigPatch {
                sandbox: Some(policy),
                ..ConfigPatch::default()
            },
            None => {
                return RemoteSlash::Note(format!(
                    "unknown sandbox `{}` (off | seatbelt | bubblewrap)",
                    s.trim()
                ))
            }
        },
        PendingCommand::Agent(name) => ConfigPatch {
            persona: Some(name.clone()),
            ..ConfigPatch::default()
        },
        // The server replaces its list on SetConfig, so send the whole
        // list (this connection's copy plus the new entry), as the local
        // path appends to its own list.
        PendingCommand::Allow(tool) => {
            let tool = tool.trim();
            if tool.is_empty() {
                return RemoteSlash::Note("usage: /allow <tool>".into());
            }
            lists.allow.push(tool.to_string());
            ConfigPatch {
                allow: Some(lists.allow.clone()),
                ..ConfigPatch::default()
            }
        }
        PendingCommand::Deny(tool) => {
            let tool = tool.trim();
            if tool.is_empty() {
                return RemoteSlash::Note("usage: /deny <tool>".into());
            }
            lists.deny.push(tool.to_string());
            ConfigPatch {
                deny: Some(lists.deny.clone()),
                ..ConfigPatch::default()
            }
        }
        _ => return RemoteSlash::Unsupported,
    };
    RemoteSlash::Patch(patch, None)
}

/// The /sandbox values, local and remote alike: exactly off, seatbelt,
/// bubblewrap (bwrap is its alias). Anything else is unknown, not off.
fn parse_sandbox_policy(mode: &str) -> Option<modron_core::SandboxPolicy> {
    match mode.trim() {
        "off" => Some(modron_core::SandboxPolicy::Off),
        "seatbelt" => Some(modron_core::SandboxPolicy::Seatbelt),
        "bubblewrap" | "bwrap" => Some(modron_core::SandboxPolicy::Bubblewrap),
        _ => None,
    }
}

/// The frame for `/remote-control`: it needs a session the driver has
/// adopted. `Err` is the user-visible note (the loop keeps running).
fn remote_control_payload(
    driver_session: Option<uuid::Uuid>,
    bridge_url: String,
) -> Result<Payload, String> {
    match driver_session {
        Some(session_id) => Ok(Payload::RemoteControl {
            session_id,
            bridge_url,
        }),
        None => Err("no session yet - send a task first, then /remote-control".into()),
    }
}

#[cfg(test)]
mod approval_state_tests {
    //! C2 (D6): per-run approval state in the run loop. Pure mapping and
    //! the RunState reset are exercised here; the terminal loop itself is
    //! not (it needs a TTY).
    use super::*;
    use modron_core::ToolCall;
    use modron_protocol::{Decision, LiveState, PendingApproval as WirePending};
    use uuid::Uuid;

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "bash".into(),
            arguments: serde_json::json!({"command": "ls"}),
        }
    }

    fn state_with_bus() -> (RunState, App, ApprovalBus) {
        let (delta_tx, delta_rx) = mpsc::channel::<DeltaOrTool>();
        let approval = ApprovalBus {
            slot: Arc::new(Mutex::new(None)),
            rest_approved: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            request_id: Arc::new(Mutex::new(None)),
        };
        let state = RunState::new(
            inert_engine(),
            true,
            delta_tx,
            delta_rx,
            approval.clone(),
            None,
            None,
        );
        (state, App::default(), approval)
    }

    #[test]
    fn snapshot_shows_the_server_mode_and_seeds_the_lists() {
        // C5: attach to a yolo session with server-side allow/deny entries.
        // The label follows the server, and a following /deny sends the
        // server's existing denies plus the new one (never a bare list).
        let (mut state, mut app, _approval) = state_with_bus();
        let (ui_tx, ui_rx) = tokio::sync::mpsc::unbounded_channel::<UiFrame>();
        state.ui_rx = ui_rx;
        let session = Uuid::new_v4();
        ui_tx
            .send(UiFrame::Snapshot(Box::new(crate::remote::AttachSnapshot {
                session_id: session,
                last_seq: 0,
                events: Vec::new(),
                live: LiveState {
                    approval_mode: Some(modron_protocol::ApprovalModeWire::ApproveAll),
                    allow: vec!["diagnostics".into()],
                    deny: vec!["bash".into()],
                    ..LiveState::default()
                },
            })))
            .unwrap();
        state.drain_ui_frames(&mut app);
        assert_eq!(app.approval_mode, "yolo");

        match remote_slash_command(
            &PendingCommand::Deny("write".into()),
            &mut state.remote_lists,
        ) {
            RemoteSlash::Patch(patch, _) => {
                assert_eq!(
                    patch.deny,
                    Some(vec!["bash".to_string(), "write".to_string()])
                );
                assert_eq!(patch.allow, None);
            }
            _ => panic!("expected a config patch"),
        }
    }

    #[test]
    fn snapshot_with_a_pending_approval_adopts_the_prompt() {
        let session = Uuid::new_v4();
        let live = LiveState {
            pending_approval: Some(WirePending {
                request_id: "req-1".into(),
                call: call("c1"),
            }),
            ..LiveState::default()
        };
        let frame = Frame::event(
            Some(session),
            3,
            Payload::Snapshot {
                session_id: session,
                last_seq: 3,
                events: Vec::new(),
                live,
            },
        );
        let inputs = frame_to_inputs(frame);
        assert_eq!(inputs.len(), 2, "snapshot, then the adopted prompt");
        assert!(matches!(inputs[0], LoopInput::Snapshot(_)));
        match &inputs[1] {
            LoopInput::Approval { request_id, call } => {
                assert_eq!(request_id, "req-1");
                assert_eq!(call.id, "c1");
            }
            _ => panic!("expected the snapshot's pending approval"),
        }
    }

    #[test]
    fn snapshot_without_a_pending_approval_adopts_nothing() {
        let session = Uuid::new_v4();
        let frame = Frame::event(
            Some(session),
            0,
            Payload::Snapshot {
                session_id: session,
                last_seq: 0,
                events: Vec::new(),
                live: LiveState::default(),
            },
        );
        let inputs = frame_to_inputs(frame);
        assert_eq!(inputs.len(), 1);
        assert!(matches!(inputs[0], LoopInput::Snapshot(_)));
    }

    #[test]
    fn approval_resolved_frame_maps_to_a_loop_input() {
        let frame = Frame::event(
            None,
            0,
            Payload::ApprovalResolved {
                request_id: "req-9".into(),
                decision: Decision::AllowOnce,
            },
        );
        let inputs = frame_to_inputs(frame);
        assert!(matches!(
            &inputs[..],
            [LoopInput::ApprovalResolved { request_id }] if request_id == "req-9"
        ));
    }

    #[test]
    fn a_released_remote_waiter_sends_nothing() {
        // Dropped locally (or resolved elsewhere): no verdict, no send.
        assert_eq!(remote_decision(None, true), None);
        assert_eq!(remote_decision(None, false), None);
        assert_eq!(remote_decision(Some(false), true), Some(Decision::Deny));
        assert_eq!(
            remote_decision(Some(true), false),
            Some(Decision::AllowOnce)
        );
        assert_eq!(remote_decision(Some(true), true), Some(Decision::AllowAll));
    }

    #[test]
    fn a_run_boundary_clears_the_app_and_the_approver_bus() {
        let (mut state, mut app, approval) = state_with_bus();
        let (respond, receiver) = mpsc::channel::<bool>();
        app.approve_rest = true;
        app.pending_approval = Some(PendingApproval {
            call: call("shown"),
            respond,
        });
        approval.rest_approved.store(true, Ordering::Relaxed);
        *approval.request_id.lock().unwrap() = Some("req-q".into());
        let (queued, _queued_rx) = mpsc::channel::<bool>();
        *approval.slot.lock().unwrap() = Some((call("queued"), queued));
        state.approval_request = Some("req-shown".into());

        state.reset_run_approvals(&mut app);

        assert!(!app.approve_rest);
        assert!(app.pending_approval.is_none());
        assert!(!approval.rest_approved.load(Ordering::Relaxed));
        assert!(approval.slot.lock().unwrap().is_none());
        assert!(approval.request_id.lock().unwrap().is_none());
        assert!(state.approval_request.is_none());
        // The shown prompt's sender was dropped: its engine is released.
        assert!(receiver.recv().is_err());
    }

    #[test]
    fn the_approver_flag_follows_the_app_each_tick() {
        let (mut state, mut app, approval) = state_with_bus();
        app.approve_rest = true;
        state.mirror_approve_rest(&mut app);
        assert!(approval.rest_approved.load(Ordering::Relaxed));
        app.approve_rest = false;
        state.mirror_approve_rest(&mut app);
        assert!(!approval.rest_approved.load(Ordering::Relaxed));
    }

    #[test]
    fn resolved_frame_clears_only_the_matching_prompt() {
        let (mut state, mut app, _approval) = state_with_bus();
        let (ui_tx, ui_rx) = tokio::sync::mpsc::unbounded_channel::<UiFrame>();
        state.ui_rx = ui_rx;
        let (respond, _rx) = mpsc::channel::<bool>();
        app.pending_approval = Some(PendingApproval {
            call: call("shown"),
            respond,
        });
        state.approval_request = Some("req-shown".into());

        ui_tx
            .send(UiFrame::ApprovalResolved {
                request_id: "req-other".into(),
            })
            .unwrap();
        state.drain_ui_frames(&mut app);
        assert!(app.pending_approval.is_some(), "another request leaves it");

        ui_tx
            .send(UiFrame::ApprovalResolved {
                request_id: "req-shown".into(),
            })
            .unwrap();
        state.drain_ui_frames(&mut app);
        assert!(app.pending_approval.is_none(), "the matching one clears");
        assert!(state.approval_request.is_none());
    }
}

#[cfg(test)]
mod remote_command_tests {
    //! C3: remote-mode slash commands. The pure mapping is tested here;
    //! the driver loop itself needs a live socket.
    use super::*;
    use modron_core::SandboxPolicy;
    use modron_protocol::Payload;
    use uuid::Uuid;

    fn note_of(slash: RemoteSlash) -> String {
        match slash {
            RemoteSlash::Note(text) => text,
            _ => panic!("expected a note"),
        }
    }

    fn patch_of(slash: RemoteSlash) -> ConfigPatch {
        match slash {
            RemoteSlash::Patch(patch, _) => patch,
            _ => panic!("expected a config patch"),
        }
    }

    #[test]
    fn sandbox_typo_sends_no_patch_and_says_so() {
        let mut lists = RemoteLists::default();
        let slash = remote_slash_command(&PendingCommand::Sandbox("bubblwrap".into()), &mut lists);
        assert_eq!(
            note_of(slash),
            "unknown sandbox `bubblwrap` (off | seatbelt | bubblewrap)"
        );
    }

    #[test]
    fn sandbox_on_is_unknown_not_off() {
        let mut lists = RemoteLists::default();
        let slash = remote_slash_command(&PendingCommand::Sandbox("on".into()), &mut lists);
        assert_eq!(
            note_of(slash),
            "unknown sandbox `on` (off | seatbelt | bubblewrap)"
        );
    }

    #[test]
    fn sandbox_known_values_map_to_their_policy() {
        assert!(matches!(
            parse_sandbox_policy("off"),
            Some(SandboxPolicy::Off)
        ));
        assert!(matches!(
            parse_sandbox_policy("seatbelt"),
            Some(SandboxPolicy::Seatbelt)
        ));
        assert!(matches!(
            parse_sandbox_policy("bubblewrap"),
            Some(SandboxPolicy::Bubblewrap)
        ));
        assert!(matches!(
            parse_sandbox_policy("bwrap"),
            Some(SandboxPolicy::Bubblewrap)
        ));
        assert!(parse_sandbox_policy("bubblwrap").is_none());
        assert!(parse_sandbox_policy("").is_none());
    }

    #[tokio::test]
    async fn local_sandbox_arm_accepts_the_same_set_as_remote() {
        // C5: the local /sandbox arm takes bwrap too, through the same
        // parser the remote arm uses.
        let mut engine = inert_engine();
        let note = run_pending_command(&mut engine, PendingCommand::Sandbox("bwrap".into())).await;
        assert_eq!(engine.config.sandbox, SandboxPolicy::Bubblewrap);
        assert!(note
            .iter()
            .any(|line| line.starts_with("sandbox: bubblewrap")));
        let _ = run_pending_command(&mut engine, PendingCommand::Sandbox("off".into())).await;
        assert_eq!(engine.config.sandbox, SandboxPolicy::Off);
        let _ = run_pending_command(&mut engine, PendingCommand::Sandbox("bubblwrap".into())).await;
        assert_eq!(
            engine.config.sandbox,
            SandboxPolicy::Off,
            "a typo changes nothing"
        );
    }

    #[test]
    fn remote_allow_sends_the_whole_list_not_one_entry() {
        let mut lists = RemoteLists::default();
        let _ = remote_slash_command(&PendingCommand::Allow("bash".into()), &mut lists);
        let patch = patch_of(remote_slash_command(
            &PendingCommand::Allow("write".into()),
            &mut lists,
        ));
        assert_eq!(
            patch.allow,
            Some(vec!["bash".to_string(), "write".to_string()])
        );
        assert!(patch.deny.is_none());
    }

    #[test]
    fn remote_deny_keeps_earlier_denials() {
        let mut lists = RemoteLists::default();
        let _ = remote_slash_command(&PendingCommand::Deny("bash".into()), &mut lists);
        let patch = patch_of(remote_slash_command(
            &PendingCommand::Deny("write".into()),
            &mut lists,
        ));
        assert_eq!(
            patch.deny,
            Some(vec!["bash".to_string(), "write".to_string()])
        );
        assert!(patch.allow.is_none());
    }

    #[test]
    fn remote_control_without_a_session_is_a_note_not_a_stop() {
        let err = remote_control_payload(None, "wss://bridge".into())
            .expect_err("no session means a note");
        assert!(err.contains("no session"), "got: {err}");
    }

    #[test]
    fn remote_control_with_a_session_builds_the_payload() {
        let id = Uuid::new_v4();
        let payload =
            remote_control_payload(Some(id), "wss://bridge".into()).expect("session adopted");
        assert!(matches!(
            payload,
            Payload::RemoteControl { session_id, .. } if session_id == id
        ));
    }
}
