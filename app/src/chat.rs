//! The chat screen: a thin render model over `ViewModelSnapshot`.
//!
//! Every string shown comes from the client core (`modron-mobile-core`),
//! already formatted: this module only chooses which field goes where and
//! splits multi-line transcript rows into one visual line each. Nothing is
//! re-formatted here (AGENTS.md shared invariants).
//!
//! Keys: both hosts route keys through [`Host::route`] (the terminal via
//! [`Host::filter`], the web via a document keydown listener) before the NSH
//! view sees them. The rule is one function, [`route_key`]: Esc interrupts;
//! y / a / n decide a pending approval; y / r decide a pending plan; Alt+Enter
//! queues; everything else reaches the composer. Enter sends (steers while a
//! run is active).
//!
//! Controls: the web host also shows tappable controls (Stop while running,
//! the approval and plan options, Queue beside the composer), because phones
//! have no Esc or Alt keys. The terminal shows the same options as text.
//!
//! Colour: every colour is a token of `theme/oculus.json` ([`token_ink`]).
//! The terminal maps those through NSH's own palette; the web draws them.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

use modron_mobile_core::view::{idle_hint, spinner_frame, RUNNING_HINT};
use modron_mobile_core::view::{scrolled_up_hint, NEW_OUTPUT_BELOW_LABEL, NO_TOOLS_NOTE};
use modron_mobile_core::Attachment;
use modron_mobile_core::{
    parse_shell_input, ApprovalModeWire, Effort, Phase, RenderedRow, SandboxPolicy,
    ViewModelSnapshot,
};
use modron_view::{TranscriptEntry, COMPOSER_PLACEHOLDER, QUEUE_LABEL, STOP_LABEL};
use nsh_core::element::{input, AdaptiveColor};
use nsh_core::prelude::*;
#[cfg(not(target_arch = "wasm32"))]
use nsh_platform_tui::{Input, Modifiers};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use uuid::Uuid;

use crate::help::HELP_PAGE;
use crate::nav::{InviteLine, Nav, NavStep, NavView, Screen};
use crate::portal::PortalRow;
use crate::slash;
use crate::suggest;
use crate::theme::Choice;
use std::collections::VecDeque;

/// What the user asked the client to do. The host or the view sends these;
/// the session driver (`crate::session`) turns them into core calls.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiAction {
    /// Composer submit: a submit when idle, a steer while a run is active
    /// (the core's `send_message`).
    Send(String),
    /// Alt+Enter (or the web Queue control): hold the message for the
    /// session's next run.
    Queue(String),
    /// Remove one queued message by position (0 = next to run). Backspace on
    /// an empty composer, or a tap on a web chip.
    Dequeue(usize),
    /// Index into the pending approval's `options` (protocol order).
    Decide(usize),
    /// Answer the proposed plan; `note` is the revise text (None when empty).
    DecidePlan {
        approve: bool,
        note: Option<String>,
    },
    Interrupt,
    /// `/compact` in the composer.
    Compact,
    /// Sessions screen: refresh the list (the core's `list_sessions`).
    ListSessions,
    /// Attach to a session from its start (the core's `attach`).
    Attach(Uuid),
    /// Create a session (the core's `new_session`).
    NewSession,
    /// Settings: one config change, sent as a command. The screen shows it
    /// when the server's ConfigChanged comes back in the snapshot.
    SetApprovalMode(ApprovalModeWire),
    SetPhase(Phase),
    SetEffort(Option<Effort>),
    SetSandbox(SandboxPolicy),
    /// A screen-navigation step: from a key (through `Host::route`) or a tap
    /// (the web header). The host applies it; it never reaches the driver.
    Nav(NavStep),
    /// `/model <name>`: switch the model (the core's `select_model`).
    SelectModel(String),
    /// Shift+S or the web save control: write the model and provider as the
    /// server's default for future sessions (the core's
    /// `persist_default_model`). The current session keeps its model. The
    /// refusal, if any, shows through the last error.
    SaveDefaultModel {
        model: String,
        provider: String,
    },
    /// The model picker opened: ask the server for the router defaults.
    ListModels,
    /// `/mode` with no argument: the next approval mode.
    CycleMode,
    /// `/reasoning`: show or hide streamed reasoning (the core's
    /// `set_show_reasoning`, toggled from the snapshot's flag).
    ToggleReasoning,
    /// `/sandbox` with no argument: the next sandbox policy.
    CycleSandbox,
    /// `/allow <tool>` and `/deny <tool>`: add the tool to the allow or deny
    /// list for future runs (the core's `set_tool_lists`, the list kept).
    AddToolRule {
        allow: bool,
        tool: String,
    },
    /// `/allow` or `/deny` with no tool: show the current lists.
    ShowToolLists,
    /// `/worktree <title>`: a task session in its own worktree (the core's
    /// `create_task`, off the default base).
    CreateTask(String),
    /// `/fork`, `/tasks`, `/share`, `/remote-control <url>`.
    Fork,
    TaskStatus,
    Share,
    RemoteControl(String),
    /// The sessions confirmation said yes.
    DeleteSession(Uuid),
    /// The agent screen's steer (Enter) and the confirmed kill.
    SteerAgent {
        agent: String,
        text: String,
    },
    KillAgent(String),
    /// A local message (unknown command, a usage line), shown as the
    /// snapshot's last error, as a refused call is.
    Notice(String),
    /// Owner portal (web). The token is sent once and never stored; the
    /// view clears its input on submit.
    SignIn(String),
    SignOut,
    /// The portal list: refresh now (also every 15 s while it shows).
    RefreshPortal,
    /// Open a same-origin URL (a portal row: the owner's `open_url`).
    OpenUrl(String),
    /// Settings share (owner, connected session): list the invites, create
    /// one with a role (`viewer` or `approver`), revoke one by id, copy a URL.
    ListInvites,
    CreateInvite(String),
    RevokeInvite(String),
    CopyInvite(String),
    /// `!command` (shared: the output may reach the model) and `!!command`
    /// (private: shown, never sent to the model). The core's `shell` and
    /// `shell_private`; a bare `!` gives the core's usage refusal.
    Shell {
        command: String,
        private: bool,
    },
    /// Ctrl+V: read an image (or the text) from the clipboard. An image is
    /// staged as a chip for the next submit (see [`stage`]).
    Paste,
    /// Backspace on an empty composer with chips staged: drop the last chip.
    Detach,
    /// The tool detail panel: Ctrl+T, or a tap on the turn summary (web).
    Tools(ToolsStep),
    /// Credentials: the core's `auth_providers` (the rows arrive in the
    /// snapshot).
    ListAuth,
    /// Credentials: start the server-side browser sign-in for a provider.
    AuthStart(String),
    /// Credentials: save a pasted API key. The key never appears in logs:
    /// [`SecretKey`] prints redacted.
    SetKey(SecretKey),
    /// `/plan <task>`: switch to the plan phase, then submit the task.
    Plan(String),
    /// `/goal <text>`: the stop condition for future turns; empty clears it.
    SetGoal(String),
    /// `/agent <name>`: switch persona for future turns.
    SetPersona(String),
    /// `/exit`, `/quit`, `/q`: leave the app. The host sets the quit flag
    /// ([`quit_requested`]) and the terminal loop exits; never sent on.
    Exit,
    /// The attach control (web): a picked image file, read by the page. The
    /// driver stages it as a chip, like Ctrl+V does on the terminal.
    AttachFile {
        name: String,
        mime: String,
        bytes: Vec<u8>,
    },
    /// `/clear`: hides the transcript rows that are on screen now, in this
    /// view only (nothing is sent). A reconnect or a session change shows
    /// them again. Handled by the host, never sent on.
    Clear,
    /// `/context`: a local note from the snapshot's context and token numbers.
    Context,
    /// Automations (`/automations`): the core's `list_automations`, then
    /// `add_automation` (a prompt on an interval) and `remove_automation`.
    ListAutomations,
    AddAutomation {
        name: String,
        prompt: String,
        interval_minutes: u32,
    },
    RemoveAutomation(String),
    /// Tasks (`/tasks`): one task's status (the core answers with a note) and
    /// the merge of its branch into the base (the core's `merge_task`).
    TaskStatusOf(Uuid),
    MergeTask(Uuid),
    /// Settings: remove a permission rule by its row index (after y); the
    /// remaining rules are sent as the whole set.
    RemoveRule(usize),
    /// Tasks: refresh the statuses of the task sessions (one core batch).
    RefreshTaskStatuses,
    /// The agent picker opened: ask for the server's personas (`personas_req`).
    ListPersonas,
    /// Credentials: remove a signed-in provider's saved credentials (`auth_remove`).
    AuthRemove(String),
    /// Settings: remove an allow (`allow`) or deny entry by its tool name.
    RemoveToolRule {
        allow: bool,
        tool: String,
    },
}

/// Set by `/exit` (see [`UiAction::Exit`]); the terminal loop checks it.
static QUIT: AtomicBool = AtomicBool::new(false);

/// Whether `/exit` was run. Only the terminal host reads it.
pub fn quit_requested() -> bool {
    QUIT.load(Ordering::SeqCst)
}

/// A pasted API key and its provider, for `auth_set_key`. Its `Debug` is
/// redacted, so a logged action never shows the key.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretKey {
    pub provider: String,
    pub key: String,
}

impl std::fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretKey")
            .field("provider", &self.provider)
            .field("key", &"<redacted>")
            .finish()
    }
}

/// One step of the tool detail panel. The panel owns the keyboard while it
/// is open (as the old TUI's ctrl+t panel does).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolsStep {
    /// Ctrl+T: open at the newest row, or close. Opening with no tool
    /// activity leaves the panel closed and sends a note instead.
    Toggle,
    /// Esc.
    Close,
    /// Up and Down move one row; PageUp and PageDown move a page.
    Move(isize),
    /// Home and End.
    Top,
    Bottom,
}

impl UiAction {
    /// The actions the web host performs itself (the owner portal's calls).
    /// They never reach the session driver.
    pub fn is_portal(&self) -> bool {
        matches!(
            self,
            UiAction::SignIn(_)
                | UiAction::SignOut
                | UiAction::RefreshPortal
                | UiAction::OpenUrl(_)
                | UiAction::ListInvites
                | UiAction::CreateInvite(_)
                | UiAction::RevokeInvite(_)
                | UiAction::CopyInvite(_)
        )
    }
}

/// Key hints for the approval options, in protocol order. Keys are TUI
/// presentation (AGENTS.md divergence table); the labels are the core's.
const DECISION_KEYS: [&str; 3] = ["y", "a", "n"];

/// Key hints for the plan options, in protocol order (approve, revise).
const PLAN_KEYS: [&str; 2] = ["y", "r"];

/// True on the web host: tappable controls are drawn there only. The
/// terminal keeps its key hints as text (AGENTS.md divergence table).
pub(crate) const WEB_CONTROLS: bool = cfg!(target_arch = "wasm32");

/// The transcript's height limits in logical pixels: it never shrinks below
/// the minimum, and on a window taller than the maximum plus the chrome it
/// stays at the maximum.
const TRANSCRIPT_MAX_H: f32 = 600.0;
const TRANSCRIPT_MIN_H: f32 = 240.0;
/// The rows below the transcript (status strips, approval, queue, composer,
/// controls) plus the header, at their usual size.
const CHROME_RESERVE_H: f32 = 330.0;

thread_local! {
    /// The sender for the page's inner height; `App` binds it and drains
    /// the receiver on the engine thread (a signal may not be set from the
    /// page's timer, which runs outside the reactive runtime).
    static WINDOW_TX: std::cell::RefCell<Option<UnboundedSender<f32>>> = const { std::cell::RefCell::new(None) };
    /// The last height sent, so the feed carries changes only.
    static WINDOW_LAST: std::cell::Cell<f32> = const { std::cell::Cell::new(f32::NAN) };
}

/// The initial window height (the terminal's default: the maximum
/// transcript plus the chrome).
pub(crate) fn window_height_default() -> f32 {
    TRANSCRIPT_MAX_H + CHROME_RESERVE_H
}

/// Opens the window height feed: returns its receiver, which `App` drains
/// into the signal the transcript height reads.
pub(crate) fn window_height_feed() -> UnboundedReceiver<f32> {
    let (tx, rx) = unbounded_channel();
    WINDOW_TX.with(|c| *c.borrow_mut() = Some(tx));
    WINDOW_LAST.with(|c| c.set(f32::NAN));
    rx
}

/// Records the page's inner height (the web host's timer calls this). Only
/// a change is sent; the engine applies it to the layout.
#[cfg(target_arch = "wasm32")]
pub(crate) fn set_window_height(h: f32) {
    if !h.is_finite() || h <= 0.0 || WINDOW_LAST.with(|c| c.get()) == h {
        return;
    }
    WINDOW_LAST.with(|c| c.set(h));
    if let Some(tx) = WINDOW_TX.with(|c| c.borrow().clone()) {
        let _ = tx.send(h);
    }
}

/// The transcript's height for the window: the window's height less the
/// chrome, clamped to the limits. Reads the bound signal, so a view that
/// calls it re-lays out when the window changes.
pub(crate) fn transcript_px(window: f32, panel_open: bool, composer_lines: usize) -> f32 {
    let panel = if panel_open { TOOL_PANEL_H } else { 0.0 };
    let growth = (composer_lines.clamp(1, COMPOSER_MAX_LINES) - 1) as f32 * COMPOSER_LINE_H;
    fit_transcript_height(window - panel - growth)
}

/// The transcript height for a window of `window` logical pixels.
pub(crate) fn fit_transcript_height(window: f32) -> f32 {
    (window - CHROME_RESERVE_H).clamp(TRANSCRIPT_MIN_H, TRANSCRIPT_MAX_H)
}

/// Visible slots for queue chips, steer echoes, plan lines and the live
/// activity rows. Fixed slot counts keep NSH's cell layout static (see the
/// transcript note below).
const QUEUE_SLOTS: usize = 4;
/// Visible image chips above the composer.
const ATTACH_SLOTS: usize = 4;
const STEER_SLOTS: usize = 3;
const PLAN_LINE_SLOTS: usize = 12;
const TOOL_SLOTS: usize = 6;
const SWARM_SLOTS: usize = 4;
/// Visible slots for the command suggestions (table rows, then local rows).
const SLASH_SLOTS: usize = 16;
const NOTE_SLOTS: usize = 2;
/// Rows of the tool detail panel shown at once (its title is one more row).
const TOOL_PANEL_SLOTS: usize = 12;
/// The panel's height in logical pixels: its title and rows, at the row size.
const TOOL_PANEL_H: f32 = (TOOL_PANEL_SLOTS + 1) as f32 * COMPOSER_LINE_H;
/// The composer grows one row per line up to this many lines, then scrolls
/// inside itself (NSH `max_h`, the same as the transcript's clamp).
pub const COMPOSER_MAX_LINES: usize = 6;
/// One composer line in logical pixels.
const COMPOSER_LINE_H: f32 = 18.0;

/// The composer text, mirrored from the view so the host can read it for a
/// revise note (the host filters keys before the composer sees them).
static DRAFT: Mutex<String> = Mutex::new(String::new());
/// Sent inputs, oldest first (this app session, the last [`HISTORY_MAX`]).
/// The view appends on submit; the host recalls with Up / Down.
static HISTORY: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// The recalled entry's index, or None while the draft is live.
static HISTORY_POS: Mutex<Option<usize>> = Mutex::new(None);
/// The draft kept while an entry is recalled, restored by Down past newest.
static HISTORY_DRAFT: Mutex<String> = Mutex::new(String::new());
/// Lines the transcript is scrolled up from the newest row (0 = at the
/// bottom, following). Set by the host's PageUp / PageDown / Home / End.
static SCROLL_OFFSET: Mutex<usize> = Mutex::new(0);
/// Bumped on every scroll step, so the view scrolls once per step.
static SCROLL_SEQ: AtomicU64 = AtomicU64::new(0);
/// True when rows arrived while the view was scrolled up.
static NEW_BELOW: AtomicBool = AtomicBool::new(false);
/// The transcript's line count, as the view last laid it out (the host
/// needs it to place the top of the transcript).
static LINE_TOTAL: AtomicUsize = AtomicUsize::new(0);

/// Sent inputs kept for recall.
use crate::clock::Instant;

pub const HISTORY_MAX: usize = 200;
/// Transcript lines a PageUp or PageDown moves.
pub const SCROLL_PAGE: usize = 10;
/// Set when a revise note was sent: the composer clears once the plan has
/// been resolved (see [`after_snapshot`]), so a refused revise keeps its note.
static CLEAR_DRAFT: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_draft(text: &str) {
    if let Ok(mut draft) = DRAFT.lock() {
        draft.clear();
        draft.push_str(text);
    }
}

fn draft_text() -> String {
    DRAFT.lock().map(|d| d.clone()).unwrap_or_default()
}

/// The composer text the engine has applied. The host reads it to route
/// Enter and Esc, and it moves on the engine thread, so a harness that types
/// waits on it (the same text the view shows once the engine commits).
pub fn composer_text() -> String {
    draft_text()
}

/// A key the host routes, named without any target's key type. The
/// terminal maps its key names with [`key_of`]; the web maps DOM keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Enter,
    Escape,
    Backspace,
    /// One printable character, as the key produced it.
    Char(char),
    /// Up and Down arrows (the sessions and settings lists).
    Up,
    Down,
    /// F2: opens Settings from any screen.
    F2,
    /// F1: help from any screen.
    F1,
    /// Tab: completes the selected command suggestion.
    Tab,
    /// Shift+Tab: flips the approval mode.
    BackTab,
    /// Left and Right arrows: the composer's caret.
    Left,
    Right,
    /// Home and End: the transcript's top and bottom when the composer is
    /// empty, the caret's line start and end otherwise.
    Home,
    End,
    /// Delete: always the composer's.
    Delete,
    /// PageUp and PageDown: page the transcript.
    PageUp,
    PageDown,
    /// Any other key (dead keys, F-keys other than F1 and F2): always the
    /// composer's.
    Other,
}

/// Maps a key name (the terminal's and the DOM's `KeyboardEvent.key`) to a
/// [`Key`]. A single character is [`Key::Char`]; every other name that is
/// not one of the three special keys is [`Key::Other`].
pub fn key_of(name: &str) -> Key {
    match name {
        "Enter" => Key::Enter,
        "Escape" => Key::Escape,
        "Backspace" => Key::Backspace,
        _ => {
            let mut chars = name.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => Key::Char(c),
                _ => Key::Other,
            }
        }
    }
}

/// Maps a key name as a host reports it, including the names only the
/// screens use (`ArrowUp`, `ArrowDown`, `F2`); everything else is
/// [`key_of`]'s. Both hosts call this.
pub fn host_key(name: &str) -> Key {
    match name {
        "ArrowUp" => Key::Up,
        "ArrowDown" => Key::Down,
        "F2" => Key::F2,
        "F1" => Key::F1,
        "Tab" => Key::Tab,
        "BackTab" => Key::BackTab,
        "ArrowLeft" => Key::Left,
        "ArrowRight" => Key::Right,
        "Home" => Key::Home,
        "End" => Key::End,
        "Delete" => Key::Delete,
        "PageUp" => Key::PageUp,
        "PageDown" => Key::PageDown,
        _ => key_of(name),
    }
}

/// The snapshot facts key routing needs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeyFacts {
    /// The Settings row under the cursor can be removed (`d` asks first).
    pub removable: bool,
    /// Option count of the pending approval, when one is pending.
    pub approval_options: Option<usize>,
    /// Whether a proposed plan waits for a decision.
    pub plan_pending: bool,
    /// Messages held for the next run (the queue chips).
    pub queued: usize,
    /// The screen on top of the stack (Chat unless another is open).
    pub screen: Screen,
    /// Whether the sessions filter has text.
    pub filter_active: bool,
    /// The session Enter would attach (Sessions only).
    pub selected_session: Option<Uuid>,
    /// The action Enter would send for the selected settings row.
    pub enter_setting: Option<UiAction>,
    /// The save-as-default action for the highlighted model (Models only).
    pub save_default: Option<UiAction>,
    /// Packet C3f: the number of command suggestions shown (0 when none).
    pub slash_len: usize,
    /// The composer text is exactly a command word.
    pub slash_exact: bool,
    /// The selected row index of the open list (Sessions, Models, Settings).
    pub selected_index: usize,
    /// The model Enter would switch to (Models only).
    pub selected_model: Option<String>,
    /// The sub-agent Ctrl+A opens.
    pub first_agent: Option<String>,
    /// Whether the open agent still takes a steer or a kill (Agent only).
    pub agent_live: bool,
    /// Whether a confirmation waits for y or n.
    pub confirming: bool,
    /// Whether a turn is running (Esc interrupts it).
    pub running: bool,
    /// Whether a sent input is recalled into the composer.
    pub recalling: bool,
    /// How many sent inputs can be recalled.
    pub history_len: usize,
    /// The tool detail panel is open (it owns the keyboard).
    pub tool_panel: bool,
    /// The suggestions are `@file` completions.
    pub file_mode: bool,
    /// The credentials screen has a key entry open (Esc closes it first).
    pub auth_keying: bool,
    /// Image chips staged for the next submit (Backspace removes one first).
    pub attachments: usize,
    /// An inline form is open on the automations or tasks screen.
    pub form_open: bool,
    /// The task Enter would attach (Tasks only).
    pub selected_task: Option<Uuid>,
    /// The approval type-ahead guard is open: a decision key that arrives
    /// now is ignored (see [`APPROVAL_KEY_GUARD`]).
    pub guarded: bool,
    /// The transcript is scrolled up (Esc on an idle composer returns it to
    /// the live tail).
    pub scrolled_up: bool,
}

/// One transcript scroll step (the keys PageUp, PageDown, Home, End).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollStep {
    Up,
    Down,
    Top,
    Bottom,
    /// A wheel or touch drag: rows toward the older end (positive) or the
    /// newer end (negative).
    Lines(isize),
}

/// Pixels per transcript row for wheel and touch scrolling (one row is
/// about one text line on the web host).
pub const WHEEL_PX_PER_ROW: f64 = 24.0;

/// The rows a wheel or touch movement of `delta_y` pixels scrolls. Scrolling
/// up (negative `delta_y`, the wheel convention) moves toward older rows, so
/// the result is positive; a movement below one row still moves one row.
pub fn rows_for_pixels(delta_y: f64) -> isize {
    if delta_y == 0.0 || !delta_y.is_finite() {
        return 0;
    }
    let rows = (delta_y.abs() / WHEEL_PX_PER_ROW).ceil().max(1.0) as isize;
    if delta_y < 0.0 {
        rows
    } else {
        -rows
    }
}

/// The approval type-ahead window: a decision key within this long of a
/// prompt first showing is ignored. The old terminal's value
/// (crates/modron-tui/src/lib.rs:538, APPROVAL_KEY_GUARD).
pub const APPROVAL_KEY_GUARD: std::time::Duration = std::time::Duration::from_millis(300);

/// The prompt on screen and when it first showed. A new request id (or a
/// plan appearing) restarts the window; the slot is cleared when no prompt
/// waits. Global because the web's tap handlers run outside the host.
static PROMPT_SHOWN: Mutex<Option<(String, Instant)>> = Mutex::new(None);

/// The identity of the prompt a snapshot shows: the front approval's request
/// id, else the plan (a plan has no id of its own).
pub fn prompt_key(snapshot: &ViewModelSnapshot) -> Option<String> {
    if let Some(p) = &snapshot.pending_approval {
        Some(format!("approval:{}", p.request_id))
    } else if snapshot.plan_pending.is_some() {
        Some("plan".to_string())
    } else {
        None
    }
}

/// Records the prompt a snapshot shows at `now`: a new key starts its window.
fn track_prompt(key: Option<String>, now: Instant) {
    let mut slot = PROMPT_SHOWN.lock().unwrap_or_else(|e| e.into_inner());
    match key {
        None => *slot = None,
        Some(key) => {
            if slot.as_ref().map(|(shown, _)| shown != &key).unwrap_or(true) {
                *slot = Some((key, now));
            }
        }
    }
}

/// Whether the type-ahead window is open at `now` (a prompt is on screen and
/// first showed less than [`APPROVAL_KEY_GUARD`] ago). The web's approval and
/// plan controls check it too.
pub fn prompt_guard_open(now: Instant) -> bool {
    PROMPT_SHOWN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|(_, shown)| now < *shown + APPROVAL_KEY_GUARD)
}

/// Where a key goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Routed {
    /// The composer gets the key.
    Composer,
    /// Up or Down recalls a sent input (the host keeps the history).
    Recall { up: bool },
    /// PageUp, PageDown, Home or End moves the transcript.
    Scroll(ScrollStep),
    /// The key became an action.
    Action(UiAction),
    /// Esc on an idle composer with text: the text goes (chips stay).
    ClearDraft,
    /// The key is a screen step (Sessions, Settings, and the Ctrl+O / F2
    /// openers).
    Nav(NavStep),
    /// Ctrl+T, and the panel's keys (see [`ToolsStep`]).
    Tools(ToolsStep),
    /// Tab on an `@file` suggestion: the selected path replaces the token.
    CompleteFile,
    /// A key the open screen has no use for: consumed, nothing happens.
    Ignored,
}

/// The one key-to-action rule, shared by every host.
///
/// Order: a pending confirmation takes y / n (other keys are consumed). F2
/// opens Settings, F1 opens Help, Ctrl+O opens Sessions and Ctrl+A opens the
/// first sub-agent, from any screen. On the other screens every key is the
/// screen's ([`route_screen_key`]). On Chat, while the command suggestions
/// show, Up / Down / Tab / Esc / Enter drive them. `?` on an empty composer
/// opens Help. Enter otherwise reaches the composer (Alt marks a queue);
/// Backspace on an empty composer with messages queued removes the last one;
/// a chord passes; Esc interrupts; with an approval pending, y / a / n decide
/// its options and every other key is the composer's; with a plan pending, y
/// approves and r revises with the trimmed composer text as the note.
///
/// Esc precedence: on Sessions and Models, Esc clears a non-empty filter
/// first and closes the screen second; on Settings, Help and Agent it closes
/// the screen; on Chat it hides the suggestions if they show, and otherwise
/// interrupts. The composer's text is passed in, so the rule is testable
/// without a view.
pub fn route_key(key: Key, alt: bool, chord: bool, facts: KeyFacts, draft: &str) -> Routed {
    let composer = Routed::Composer;
    if matches!(facts.screen, Screen::SignIn | Screen::Portal) {
        return route_portal_key(key, chord, &facts);
    }
    if facts.tool_panel && facts.screen == Screen::Chat {
        return route_tool_panel_key(key, chord);
    }
    if facts.confirming && !chord {
        return match key {
            Key::Char('y') => Routed::Nav(NavStep::Confirm),
            Key::Char('n') | Key::Escape => Routed::Nav(NavStep::Cancel),
            _ => Routed::Ignored,
        };
    }
    // Type-ahead guard (old TUI lib.rs:962-973): a decision key that arrives
    // while the prompt is fresh was typed before it was read. It is consumed,
    // not passed to the composer, and the prompt stays.
    if facts.guarded && facts.screen == Screen::Chat && !chord && is_decision_key(key, &facts) {
        return Routed::Ignored;
    }
    match key {
        Key::F2 => return Routed::Nav(NavStep::Open(Screen::Settings)),
        Key::F1 => return Routed::Nav(NavStep::Open(Screen::Help)),
        Key::Char('t') if chord && facts.screen == Screen::Chat => {
            return Routed::Tools(ToolsStep::Toggle)
        }
        Key::Char('v') if chord && facts.screen == Screen::Chat => {
            return Routed::Action(UiAction::Paste)
        }
        Key::Char('o') if chord => return Routed::Nav(NavStep::Open(Screen::Sessions)),
        Key::Char('a') if chord && facts.screen == Screen::Chat => {
            return match facts.first_agent.clone() {
                Some(id) => Routed::Nav(NavStep::OpenAgent(id)),
                None => composer,
            };
        }
        _ => {}
    }
    if facts.screen != Screen::Chat {
        return route_screen_key(key, chord, &facts, draft);
    }
    let suggesting = facts.slash_len > 0;
    let single_line = !draft.contains('\n');
    let recall = !suggesting && single_line && (draft.is_empty() || facts.recalling);
    match key {
        Key::Escape if chord => return composer,
        Key::Escape => return escape_route(&facts, draft),
        Key::Enter if suggesting && !facts.slash_exact && !facts.file_mode => {
            return Routed::Nav(NavStep::CompleteSelected)
        }
        Key::Enter if alt => return queue_route(draft),
        Key::Enter => return composer,
        Key::Tab if suggesting && facts.file_mode => return Routed::CompleteFile,
        Key::Tab if suggesting => return Routed::Nav(NavStep::CompleteSelected),
        Key::Up if suggesting => return Routed::Nav(NavStep::SlashMove(-1)),
        Key::Down if suggesting => return Routed::Nav(NavStep::SlashMove(1)),
        Key::Up if recall => {
            return if facts.history_len == 0 && !facts.recalling {
                Routed::Ignored
            } else {
                Routed::Recall { up: true }
            }
        }
        Key::Down if recall => return Routed::Recall { up: false },
        Key::Char('?') if !chord && draft.is_empty() => {
            return Routed::Nav(NavStep::Open(Screen::Help))
        }
        Key::Backspace if !chord && draft.is_empty() && facts.attachments > 0 => {
            return Routed::Action(UiAction::Detach);
        }
        Key::Backspace if !chord && draft.is_empty() && facts.queued > 0 => {
            return Routed::Action(UiAction::Dequeue(facts.queued - 1));
        }
        Key::PageUp if !chord => return Routed::Scroll(ScrollStep::Up),
        Key::PageDown if !chord => return Routed::Scroll(ScrollStep::Down),
        Key::Home if !chord && draft.is_empty() => return Routed::Scroll(ScrollStep::Top),
        Key::End if !chord && draft.is_empty() => return Routed::Scroll(ScrollStep::Bottom),
        Key::BackTab if !chord => return Routed::Action(UiAction::CycleMode),
        Key::Other
        | Key::Backspace
        | Key::Up
        | Key::Down
        | Key::Tab
        | Key::F1
        | Key::F2
        | Key::Left
        | Key::Right
        | Key::Delete
        | Key::Home
        | Key::End
        | Key::PageUp
        | Key::PageDown
        | Key::BackTab => return composer,
        _ if chord => return composer,
        Key::Char(_) => {}
    }
    let Key::Char(c) = key else { return composer };
    let name = c.to_string();
    if let Some(count) = facts.approval_options {
        return match DECISION_KEYS.iter().position(|k| *k == name) {
            Some(index) if index < count => Routed::Action(UiAction::Decide(index)),
            _ => composer,
        };
    }
    if facts.plan_pending {
        if name == PLAN_KEYS[0] {
            return Routed::Action(UiAction::DecidePlan { approve: true, note: None });
        }
        if name == PLAN_KEYS[1] {
            return Routed::Action(plan_answer_for(false, draft));
        }
    }
    composer
}

/// Whether a key decides the pending approval (y, a, n, Esc = deny) or the
/// pending plan (y, r). Only the keys the router would act on.
fn is_decision_key(key: Key, facts: &KeyFacts) -> bool {
    match key {
        Key::Escape => facts.approval_options.is_some(),
        Key::Char(c) => {
            let name = c.to_string();
            (facts.approval_options.is_some() && DECISION_KEYS.contains(&name.as_str()))
                || (facts.plan_pending && PLAN_KEYS.contains(&name.as_str()))
        }
        _ => false,
    }
}

/// The keys while the tool detail panel is open: Ctrl+T and Esc close it,
/// Up and Down move a row, PageUp and PageDown a page, Home and End jump.
/// Any other key is consumed.
fn route_tool_panel_key(key: Key, chord: bool) -> Routed {
    if chord {
        return match key {
            Key::Char('t') => Routed::Tools(ToolsStep::Toggle),
            _ => Routed::Ignored,
        };
    }
    match key {
        Key::Escape => Routed::Tools(ToolsStep::Close),
        Key::Up => Routed::Tools(ToolsStep::Move(-1)),
        Key::Down => Routed::Tools(ToolsStep::Move(1)),
        Key::PageUp => Routed::Tools(ToolsStep::Move(-(SCROLL_PAGE as isize))),
        Key::PageDown => Routed::Tools(ToolsStep::Move(SCROLL_PAGE as isize)),
        Key::Home => Routed::Tools(ToolsStep::Top),
        Key::End => Routed::Tools(ToolsStep::Bottom),
        _ => Routed::Ignored,
    }
}

/// The action a submitted composer line means. A slash command runs its
/// command; `!command` is a shell command (see [`shell_command`]); anything
/// else is a task.
pub fn submit_action(task: &str) -> UiAction {
    if task.starts_with('/') {
        return slash::run(task);
    }
    match parse_shell_input(task) {
        Some(input) => UiAction::Shell { command: input.command, private: input.private },
        None => UiAction::Send(task.to_string()),
    }
}

/// Esc, in precedence order: a pending approval is denied (its last option,
/// the third decision); a pending plan stays pending; open command suggestions
/// close; a running turn is interrupted; an idle composer with text is
/// cleared; a scrolled-up transcript returns to the live tail; otherwise
/// nothing happens (staged image chips are never cleared by Esc).
fn escape_route(facts: &KeyFacts, draft: &str) -> Routed {
    if let Some(count) = facts.approval_options {
        return match count {
            0 => Routed::Ignored,
            n => Routed::Action(UiAction::Decide(n.min(DECISION_KEYS.len()) - 1)),
        };
    }
    if facts.plan_pending {
        return Routed::Ignored;
    }
    if facts.slash_len > 0 {
        return Routed::Nav(NavStep::SlashHide);
    }
    if facts.running {
        return Routed::Action(UiAction::Interrupt);
    }
    if !draft.is_empty() {
        return Routed::ClearDraft;
    }
    // An idle, empty composer with the transcript scrolled up: back to the
    // live tail (old TUI: "esc live", lib.rs:3327-3329).
    if facts.scrolled_up {
        return Routed::Scroll(ScrollStep::Bottom);
    }
    Routed::Ignored
}

/// Alt+Enter: queue the composer text for the next run. An empty draft, or a
/// command (commands never queue), is consumed without effect.
fn queue_route(draft: &str) -> Routed {
    let text = draft.trim();
    if text.is_empty() || text.starts_with('/') {
        return Routed::Ignored;
    }
    Routed::Action(UiAction::Queue(text.to_string()))
}

/// Keys on the portal screens. The sign-in input is the page's own input, so
/// its keys (the token, Enter, paste) go to the composer; Esc backs out of
/// nothing there. The list types a filter, arrows move, Enter opens the
/// selected row and Esc clears the filter. Chords always reach the page.
fn route_portal_key(key: Key, chord: bool, facts: &KeyFacts) -> Routed {
    let composer = Routed::Composer;
    if chord {
        return composer;
    }
    if facts.screen == Screen::SignIn {
        return match key {
            Key::Escape => Routed::Ignored,
            _ => composer,
        };
    }
    match key {
        Key::Escape if facts.filter_active => Routed::Nav(NavStep::ClearFilter),
        Key::Escape => Routed::Ignored,
        Key::Enter => Routed::Nav(NavStep::OpenPortal(facts.selected_index)),
        Key::Up => Routed::Nav(NavStep::Move(-1)),
        Key::Down => Routed::Nav(NavStep::Move(1)),
        Key::Backspace => Routed::Nav(NavStep::FilterBack),
        Key::Char(c) => Routed::Nav(NavStep::Filter(c)),
        _ => Routed::Ignored,
    }
}

/// Keys on the screens above Chat. Help closes on Esc. Models filters by
/// typing and picks on Enter. Agent takes the steer text (the composer), and
/// `k` on an empty draft asks to kill a live agent. Sessions and Settings
/// have no composer: arrows move, Enter activates, Esc backs out, and on
/// Sessions the printable keys type the filter (`n`, `j`, `k` and `d` act
/// only while the filter is empty). A chord still passes.
fn route_screen_key(key: Key, chord: bool, facts: &KeyFacts, draft: &str) -> Routed {
    let composer = Routed::Composer;
    if chord {
        return composer;
    }
    match facts.screen {
        Screen::Help => {
            return match key {
                Key::Escape => Routed::Nav(NavStep::Back),
                Key::Tab | Key::Right => Routed::Nav(NavStep::HelpCycle(1)),
                Key::BackTab | Key::Left => Routed::Nav(NavStep::HelpCycle(-1)),
                Key::Up => Routed::Nav(NavStep::HelpScroll(-1)),
                Key::Down => Routed::Nav(NavStep::HelpScroll(1)),
                Key::PageUp => Routed::Nav(NavStep::HelpScroll(-(HELP_PAGE as isize))),
                Key::PageDown => Routed::Nav(NavStep::HelpScroll(HELP_PAGE as isize)),
                Key::Home => Routed::Nav(NavStep::HelpEdge(true)),
                Key::End => Routed::Nav(NavStep::HelpEdge(false)),
                _ => Routed::Ignored,
            };
        }
        Screen::Picker => {
            return match key {
                Key::Escape if facts.filter_active => Routed::Nav(NavStep::ClearFilter),
                Key::Escape => Routed::Nav(NavStep::Back),
                Key::Enter => Routed::Nav(NavStep::PickRow(facts.selected_index)),
                Key::Up => Routed::Nav(NavStep::Move(-1)),
                Key::Down => Routed::Nav(NavStep::Move(1)),
                Key::Backspace => Routed::Nav(NavStep::FilterBack),
                Key::Char(c) => Routed::Nav(NavStep::Filter(c)),
                _ => Routed::Ignored,
            };
        }
        // `d` removes the selected rule, allow or deny entry (after y).
        Screen::Settings if matches!(key, Key::Char('d')) && facts.removable => {
            return Routed::Nav(NavStep::AskSettingRemove(facts.selected_index));
        }
        Screen::Agent => {
            return match key {
                Key::Escape => Routed::Nav(NavStep::Back),
                // An ended agent takes no steer and no kill: its keys are consumed.
                _ if !facts.agent_live => Routed::Ignored,
                Key::Char('k') if draft.is_empty() => Routed::Nav(NavStep::AskKill),
                Key::Enter | Key::Char(_) | Key::Backspace | Key::Other => composer,
                _ => Routed::Ignored,
            };
        }
        Screen::Models => {
            return match key {
                Key::Escape if facts.filter_active => Routed::Nav(NavStep::ClearFilter),
                Key::Escape => Routed::Nav(NavStep::Back),
                Key::Enter => match &facts.selected_model {
                    Some(model) => Routed::Nav(NavStep::PickModel(model.clone())),
                    None => Routed::Ignored,
                },
                Key::Up => Routed::Nav(NavStep::Move(-1)),
                Key::Down => Routed::Nav(NavStep::Move(1)),
                Key::Backspace => Routed::Nav(NavStep::FilterBack),
                // Shift+S saves the highlighted model as the default (the old
                // terminal's key). It does not type into the filter.
                Key::Char('S') => match &facts.save_default {
                    Some(action) => Routed::Action(action.clone()),
                    None => Routed::Ignored,
                },
                Key::Char(c) => Routed::Nav(NavStep::Filter(c)),
                _ => Routed::Ignored,
            };
        }
        Screen::Automations | Screen::Tasks => return route_list_screen_key(key, facts),
        Screen::Auth => {
            return match key {
                Key::Escape if facts.auth_keying => Routed::Nav(NavStep::AuthCancelKey),
                Key::Escape => Routed::Nav(NavStep::Back),
                // The masked key input has the focus: its keys are its own.
                _ if facts.auth_keying => composer,
                Key::Enter => Routed::Nav(NavStep::Activate),
                Key::Up => Routed::Nav(NavStep::Move(-1)),
                Key::Down => Routed::Nav(NavStep::Move(1)),
                Key::Char('d') => Routed::Nav(NavStep::AskAuthRemove(facts.selected_index)),
                _ => Routed::Ignored,
            };
        }
        Screen::Chat | Screen::Sessions | Screen::Settings | Screen::SignIn | Screen::Portal => {}
    }
    let sessions = facts.screen == Screen::Sessions;
    match key {
        Key::Escape if facts.filter_active => Routed::Nav(NavStep::ClearFilter),
        Key::Escape => Routed::Nav(NavStep::Back),
        Key::Enter if sessions => match facts.selected_session {
            Some(id) => Routed::Nav(NavStep::Attach(id)),
            None => Routed::Ignored,
        },
        Key::Enter => match &facts.enter_setting {
            // Navigation steps (a picker, Auth, Models) are applied by the host.
            Some(UiAction::Nav(step)) => Routed::Nav(step.clone()),
            Some(action) => Routed::Action(action.clone()),
            None => Routed::Ignored,
        },
        Key::Up => Routed::Nav(NavStep::Move(-1)),
        Key::Down => Routed::Nav(NavStep::Move(1)),
        Key::Backspace => Routed::Nav(NavStep::FilterBack),
        Key::Char('n') if sessions && !facts.filter_active => Routed::Nav(NavStep::NewSession),
        Key::Char('d') if sessions && !facts.filter_active => {
            Routed::Nav(NavStep::AskDelete(facts.selected_index))
        }
        Key::Char('j') if sessions && !facts.filter_active => Routed::Nav(NavStep::Move(1)),
        Key::Char('k') if sessions && !facts.filter_active => Routed::Nav(NavStep::Move(-1)),
        Key::Char(c) if sessions => Routed::Nav(NavStep::Filter(c)),
        _ => Routed::Ignored,
    }
}

/// Keys on the automations and tasks screens. An open form takes its own
/// keys: Enter moves to the next field (and submits on the last), Tab moves
/// on, Backspace edits, Esc closes it. Otherwise the list types a filter (Esc
/// clears it first), arrows move, and the letters act only while the filter
/// is empty: `a` adds and `d` removes an automation; `n` creates, `m` merges,
/// and Enter attaches a task.
fn route_list_screen_key(key: Key, facts: &KeyFacts) -> Routed {
    use crate::nav::FormKind;
    if facts.form_open {
        return match key {
            Key::Escape => Routed::Nav(NavStep::FormCancel),
            Key::Enter => Routed::Nav(NavStep::FormEnter),
            Key::Tab => Routed::Nav(NavStep::FormNext),
            Key::Backspace => Routed::Nav(NavStep::FormBack),
            Key::Char(c) => Routed::Nav(NavStep::FormChar(c)),
            _ => Routed::Ignored,
        };
    }
    let automations = facts.screen == Screen::Automations;
    let tasks = facts.screen == Screen::Tasks;
    match key {
        Key::Escape if facts.filter_active => Routed::Nav(NavStep::ClearFilter),
        Key::Escape => Routed::Nav(NavStep::Back),
        Key::Up => Routed::Nav(NavStep::Move(-1)),
        Key::Down => Routed::Nav(NavStep::Move(1)),
        Key::Backspace => Routed::Nav(NavStep::FilterBack),
        Key::Enter if tasks => match facts.selected_task {
            Some(id) => Routed::Nav(NavStep::Attach(id)),
            None => Routed::Ignored,
        },
        Key::Char('a') if automations && !facts.filter_active => {
            Routed::Nav(NavStep::OpenForm(FormKind::Automation))
        }
        Key::Char('d') if automations && !facts.filter_active => {
            Routed::Nav(NavStep::AskRemove(facts.selected_index))
        }
        Key::Char('n') if tasks && !facts.filter_active => {
            Routed::Nav(NavStep::OpenForm(FormKind::Task))
        }
        Key::Char('m') if tasks && !facts.filter_active => {
            Routed::Nav(NavStep::AskMerge(facts.selected_index))
        }
        Key::Char(c) => Routed::Nav(NavStep::Filter(c)),
        _ => Routed::Ignored,
    }
}

/// The plan answer for `approve`. A revise carries the trimmed draft as its
/// note (None when empty).
fn plan_answer_for(approve: bool, draft: &str) -> UiAction {
    let note = draft.trim();
    let note = (!approve && !note.is_empty()).then(|| note.to_string());
    UiAction::DecidePlan { approve, note }
}

/// The plan answer the user gave now (key or tap). A revise with a note
/// arms the composer clear for the next snapshot where the plan is resolved.
pub fn plan_answer(approve: bool) -> UiAction {
    let action = plan_answer_for(approve, &draft_text());
    arm_plan_clear(&action);
    action
}

fn arm_plan_clear(action: &UiAction) {
    if let UiAction::DecidePlan { note: Some(_), .. } = action {
        CLEAR_DRAFT.store(true, Ordering::SeqCst);
    }
}

/// The visual style of a transcript row kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    Plain,
    Bold,
    Meta,
    /// Tool activity and sub-agent rows.
    Tool,
    /// Shell output lines (dim; the command header is `Bold`).
    Dim,
}

/// One visual line of the transcript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub style: Style,
    pub text: String,
}

/// The pending approval as the screen shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Approval {
    pub tool: String,
    pub args_summary: String,
    pub labels: Vec<String>,
}

/// The proposed plan as the screen shows it: its checkbox lines and the
/// two option labels (core copy, protocol order).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanBlock {
    pub lines: Vec<String>,
    pub labels: Vec<String>,
}

/// Everything the screen renders, derived 1:1 from one snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Model {
    pub connection: String,
    pub provider: String,
    pub running: String,
    /// Whether a turn is running (the Stop control shows then).
    pub is_running: bool,
    /// The core's full status line (model, tokens, cache, mode, phase, context).
    pub status_line: String,
    pub lines: Vec<Line>,
    pub streaming: String,
    pub approval: Option<Approval>,
    /// Messages held for the next run, front first (the queue chips).
    pub queued: Vec<String>,
    /// Steers accepted but not yet persisted as tasks (dim echoes).
    pub pending_steers: Vec<String>,
    pub plan: Option<PlanBlock>,
    /// The live turn summary (`⏺ 21 tools · glob ×9 …`), when tools ran.
    pub turn_summary: Option<String>,
    /// The live turn's tool rows, one per event, oldest first.
    pub tool_rows: Vec<String>,
    /// The live sub-agent rows (`⏵ [label] running · …`).
    pub swarm_rows: Vec<String>,
    /// The core's last note and last error, in that order.
    pub notes: Vec<String>,
    /// The live reasoning tail (the last two lines), when reasoning display
    /// is on; empty otherwise.
    pub reasoning: String,
    /// The short session id (the first eight characters), or empty.
    pub session: String,
    /// The tool detail rows (the live turn's, else the last group's) and
    /// their title, verbatim from the snapshot.
    pub tool_detail: Vec<String>,
    pub tool_title: String,
    /// The staged image chips (`[img name · size]`), above the composer.
    pub attachments: Vec<String>,
    /// The standing goal (`[goal: ...]`), empty when none is set.
    pub goal: String,
    /// The running spinner frame (empty when idle, so it stops).
    pub spinner: String,
    /// The header hint: the interrupt hint while running, else the rotating
    /// idle hint while the composer is empty (empty otherwise).
    pub hint: String,
}

impl Model {
    pub fn from_snapshot(s: &ViewModelSnapshot) -> Self {
        Self {
            attachments: staged_labels(),
            goal: modron_mobile_core::view::goal_label(&s.goal),
            spinner: spinner_text(s.running, ANIM_TICK.load(Ordering::SeqCst)),
            hint: hint_text(
                s.running,
                composer_text().is_empty(),
                ANIM_TICK.load(Ordering::SeqCst),
            ),
            connection: s.connection_label.clone(),
            provider: s.provider.clone(),
            running: s.running_label.clone(),
            is_running: s.running,
            status_line: s.status_line.clone(),
            lines: if s.history.is_empty() && !s.running {
                // A blank idle session shows the welcome banner; the first
                // row (the task echo, a note) replaces it.
                let welcome = vec![RenderedRow {
                    lines: modron_mobile_core::view::welcome_lines(
                        &s.model,
                        env!("CARGO_PKG_VERSION"),
                    ),
                    entry: TranscriptEntry::Model {
                        model: s.model.clone(),
                        reason: "welcome".into(),
                    },
                }];
                transcript_lines(&welcome)
            } else {
                transcript_lines(&s.history)
            },
            streaming: s.streaming_text.clone(),
            approval: s.pending_approval.as_ref().map(|p| Approval {
                tool: p.tool.clone(),
                args_summary: p.args_summary.clone(),
                labels: p.options.iter().map(|o| o.label.clone()).collect(),
            }),
            queued: s.queued.clone(),
            pending_steers: s.pending_steers.clone(),
            plan: s.plan_pending.as_ref().map(|p| PlanBlock {
                lines: p.lines.clone(),
                labels: p.options.iter().map(|o| o.label.clone()).collect(),
            }),
            turn_summary: s.turn_summary_line.clone(),
            tool_rows: s.tool_lines.clone(),
            swarm_rows: s.subagent_rows.clone(),
            notes: s.last_note.iter().chain(s.last_error.iter()).cloned().collect(),
            reasoning: if s.show_reasoning {
                reasoning_tail(&s.streaming_reasoning, 2)
            } else {
                String::new()
            },
            tool_detail: s.tool_detail.clone(),
            tool_title: s.tool_detail_title.clone(),
            session: s
                .session_id
                .map(|id| id.to_string().chars().take(8).collect())
                .unwrap_or_default(),
        }
    }
}

/// The last `count` lines of `text` (the live reasoning tail).
fn reasoning_tail(text: &str, count: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(count)..].join("\n")
}

/// The visual style of a transcript row kind.
pub fn style_of(entry: &TranscriptEntry) -> Style {
    match entry {
        TranscriptEntry::Task { .. } => Style::Bold,
        TranscriptEntry::ToolSummary { .. } | TranscriptEntry::Swarm { .. } => Style::Tool,
        // A shell row: the `$ command` line is prominent (see transcript_lines),
        // its output dim.
        TranscriptEntry::Shell { .. } => Style::Tool,
        TranscriptEntry::Model { .. }
        | TranscriptEntry::Usage { .. }
        | TranscriptEntry::Compaction { .. } => Style::Meta,
        TranscriptEntry::User { .. }
        | TranscriptEntry::Assistant { .. }
        | TranscriptEntry::Plan { .. }
        | TranscriptEntry::GenerativeUi { .. }
        | TranscriptEntry::Review { .. } => Style::Plain,
    }
}

/// Splits each rendered row into its visual lines, keeping row order.
pub fn transcript_lines(history: &[RenderedRow]) -> Vec<Line> {
    history
        .iter()
        .flat_map(|row| {
            let style = style_of(&row.entry);
            let shell = matches!(row.entry, TranscriptEntry::Shell { .. });
            row.lines.iter().enumerate().map(move |(index, text)| {
                // Shell rows: the header line (`$ command`, with the shared
                // private mark) is bold, the output lines are dim.
                let style = match (shell, index) {
                    (true, 0) => Style::Bold,
                    (true, _) => Style::Dim,
                    _ => style,
                };
                Line { style, text: text.clone() }
            })
        })
        .collect()
}

/// How long a note stays in the status area: the old terminal's toast
/// (`TOAST_TICKS` = 40 ticks at its 50 ms poll, crates/modron-tui/src/lib.rs).
/// `last_error` is not a note and never expires.
pub const NOTE_TTL: std::time::Duration = std::time::Duration::from_secs(2);

/// Whether a note that appeared at `since` has expired by `now`.
pub fn note_expired(since: Instant, now: Instant) -> bool {
    now >= since + NOTE_TTL
}

/// The snapshot the view shows: the first `cleared` transcript rows are
/// dropped (`/clear`), and an expired note is removed. Errors are never
/// touched.
pub fn shown_copy(raw: &ViewModelSnapshot, cleared: usize, note_gone: bool) -> ViewModelSnapshot {
    let mut shown = raw.clone();
    let floor = cleared.min(shown.history.len());
    shown.history.drain(..floor);
    if note_gone {
        shown.last_note = None;
    }
    shown
}

/// Clipboard text for the composer (Ctrl+V with no image): the driver reads
/// the clipboard and offers the text here; the host appends it on its next
/// pass (see `Host::try_action`).
static OFFERED_TEXT: Mutex<Option<String>> = Mutex::new(None);

pub fn offer_text(text: String) {
    if let Ok(mut slot) = OFFERED_TEXT.lock() {
        *slot = Some(text);
    }
}

fn take_offered_text() -> Option<String> {
    OFFERED_TEXT.lock().ok().and_then(|mut slot| slot.take())
}

/// The view's end of the link, handed to `App` through `CLIENT`.
pub struct Client {
    pub snapshots: UnboundedReceiver<ViewModelSnapshot>,
    pub actions: UnboundedSender<UiAction>,
    /// The navigation state the view renders; sent whenever it changes.
    pub nav: UnboundedReceiver<NavView>,
}

/// The host's end of the link: pushes snapshots, takes actions, and routes
/// the keys that the view must not see. It owns the screen stack and the
/// sessions and settings state, because the keys that drive them are routed
/// here.
pub struct Host {
    snapshots: UnboundedSender<ViewModelSnapshot>,
    actions: UnboundedReceiver<UiAction>,
    loopback: UnboundedSender<UiAction>,
    navs: UnboundedSender<NavView>,
    /// Option count of the pending approval, when one is pending.
    pending: Option<usize>,
    /// Whether a proposed plan waits for a decision.
    plan_pending: bool,
    /// Messages held for the next run (the queue chips).
    queued: usize,
    /// Whether a turn is running (Esc interrupts it).
    running: bool,
    /// Rows the snapshot held at the last push (new output while scrolled).
    seen_rows: usize,
    nav: Nav,
    /// The owner portal's actions, taken by the web host (`take_portal`).
    portal: VecDeque<UiAction>,
    /// Rows of the tool detail the snapshot held at the last push.
    tool_len: usize,
    /// The latest snapshot as the driver sent it; the shown copy is derived
    /// from it (see [`shown_copy`]).
    raw: Option<ViewModelSnapshot>,
    /// Transcript rows `/clear` hid (a count from the start of the history).
    clear_floor: usize,
    /// The session the floor belongs to (a change resets it).
    view_session: Option<Uuid>,
    /// The last note's text, when it appeared, and whether it has expired.
    note_text: Option<String>,
    note_since: Instant,
    note_gone: bool,
    /// A local note (`/context`) and when it was shown; it wins the note slot
    /// for [`NOTE_TTL`] and then expires like the core's notes.
    local_note: Option<(String, Instant)>,
    /// The tool detail panel's first visible row, while it is open.
    tool_panel: Option<usize>,
}

/// Creates the two ends of the link. Also resets the composer mirror, so a
/// new view starts from an empty draft.
pub fn link() -> (Host, Client) {
    set_draft("");
    CLEAR_DRAFT.store(false, Ordering::SeqCst);
    if let Ok(mut history) = HISTORY.lock() {
        history.clear();
    }
    set_history_pos(None);
    set_scroll_offset(0);
    track_prompt(None, Instant::now());
    QUIT.store(false, Ordering::SeqCst);
    NEW_BELOW.store(false, Ordering::SeqCst);
    LINE_TOTAL.store(0, Ordering::SeqCst);
    let (snap_tx, snap_rx) = unbounded_channel();
    let (act_tx, act_rx) = unbounded_channel();
    let (nav_tx, nav_rx) = unbounded_channel();
    (
        Host {
            snapshots: snap_tx,
            actions: act_rx,
            loopback: act_tx.clone(),
            navs: nav_tx,
            pending: None,
            plan_pending: false,
            queued: 0,
            running: false,
            seen_rows: 0,
            nav: Nav::default(),
            portal: VecDeque::new(),
            tool_len: 0,
            tool_panel: None,
            raw: None,
            clear_floor: 0,
            view_session: None,
            note_text: None,
            note_since: Instant::now(),
            note_gone: false,
            local_note: None,
        },
        Client { snapshots: snap_rx, actions: act_tx, nav: nav_rx },
    )
}

impl Host {
    /// Sends a snapshot to the view and records what the key routing needs:
    /// whether an approval or a plan waits, and the sessions and settings.
    pub fn push(&mut self, snapshot: &ViewModelSnapshot) {
        self.push_at(snapshot, Instant::now());
    }

    /// [`Host::push`] at the injected time `now` (starts the prompt's
    /// type-ahead window when the prompt is new).
    pub fn push_at(&mut self, snapshot: &ViewModelSnapshot, now: Instant) {
        track_prompt(prompt_key(snapshot), now);
        self.pending = snapshot.pending_approval.as_ref().map(|p| p.options.len());
        self.plan_pending = snapshot.plan_pending.is_some();
        self.queued = snapshot.queued.len();
        self.running = snapshot.running;
        self.tool_len = snapshot.tool_detail.len();
        if self.tool_len == 0 {
            self.tool_panel = None;
        }
        let rows = snapshot.history.len();
        if rows > self.seen_rows && scroll_offset() > 0 {
            NEW_BELOW.store(true, Ordering::SeqCst);
        }
        self.seen_rows = rows;
        self.bookkeeping(snapshot);
        self.raw = Some(snapshot.clone());
        let shown = self.presented(snapshot);
        self.nav.observe(&shown);
        for action in self.nav.first_run(&shown) {
            self.act(action);
        }
        let _ = self.snapshots.send(shown);
        self.notify();
    }

    /// Resets the `/clear` floor when the session changes or the connection
    /// is not up (a reconnect shows the rows again), and restarts the note's
    /// clock when its text changes.
    fn bookkeeping(&mut self, raw: &ViewModelSnapshot) {
        if raw.session_id != self.view_session
            || !matches!(raw.connection, modron_mobile_core::ConnectionState::Connected)
        {
            self.clear_floor = 0;
        }
        self.view_session = raw.session_id;
        if raw.last_note != self.note_text {
            self.note_text = raw.last_note.clone();
            self.note_since = Instant::now();
            self.note_gone = false;
        }
    }

    fn presented(&self, raw: &ViewModelSnapshot) -> ViewModelSnapshot {
        let mut shown = shown_copy(raw, self.clear_floor, self.note_gone);
        if let Some((text, _)) = &self.local_note {
            shown.last_note = Some(text.clone());
        }
        shown
    }

    /// Sends the latest snapshot again (the note expired, or `/clear` ran).
    /// One animation step at the injected time `now`. A step is due every
    /// [`ANIM_PERIOD`]. It advances the spinner while a turn runs and the
    /// idle hint while the composer is empty, and republishes only when the
    /// header changes (including the hint clearing once the user types).
    /// Returns whether it republished.
    pub fn tick(&mut self, now: Instant) -> bool {
        // The Tasks screen re-asks for its statuses every few seconds (the
        // request goes to the driver through the host's own loopback).
        if self.nav.task_refresh_due(now) {
            let _ = self.loopback.send(UiAction::RefreshTaskStatuses);
        }
        {
            let mut next = ANIM_NEXT.lock().unwrap_or_else(|e| e.into_inner());
            match *next {
                None => {
                    *next = Some(now + ANIM_PERIOD);
                    return false;
                }
                Some(due) if now < due => return false,
                Some(_) => *next = Some(now + ANIM_PERIOD),
            }
        }
        let typing = !composer_text().is_empty();
        let animating = self.running || !typing;
        if !animating && !HINT_SHOWN.load(Ordering::SeqCst) {
            return false;
        }
        if animating {
            ANIM_TICK.fetch_add(1, Ordering::SeqCst);
        }
        HINT_SHOWN.store(!self.running && !typing, Ordering::SeqCst);
        self.republish();
        true
    }

    /// [`Host::tick`] on the app clock (the terminal loop and the web frame).
    pub fn tick_now(&mut self) -> bool {
        self.tick(Instant::now())
    }

    fn republish(&mut self) {
        let Some(raw) = self.raw.clone() else {
            return;
        };
        let shown = self.presented(&raw);
        self.nav.observe(&shown);
        let _ = self.snapshots.send(shown);
        self.notify();
    }

    /// Expires the last note after [`NOTE_TTL`]; `last_error` never expires.
    fn expire_note(&mut self) {
        if self.local_note.as_ref().is_some_and(|(_, since)| note_expired(*since, Instant::now())) {
            self.local_note = None;
            self.republish();
        }
        if self.note_gone
            || self.note_text.is_none()
            || !note_expired(self.note_since, Instant::now())
        {
            return;
        }
        self.note_gone = true;
        self.republish();
    }

    /// Esc on an idle composer: the text goes and history browsing resets.
    /// The composer empties the way a queued message's does (`nav.set_text`).
    fn clear_draft(&mut self) {
        set_draft("");
        set_history_pos(None);
        self.nav.set_text("");
        self.notify();
    }

    /// `/context`: the gauge and token numbers from the last snapshot, shown
    /// as a local note for [`NOTE_TTL`].
    fn context_note(&mut self) {
        let text = match self.raw.as_ref() {
            Some(raw) => modron_mobile_core::view::context_note(
                &raw.context_label,
                raw.cumulative_tokens,
                raw.session_cache_pct,
            ),
            None => modron_mobile_core::view::context_note("", 0, None),
        };
        self.local_note = Some((text, Instant::now()));
        self.republish();
    }

    /// `/clear`: hides the rows on screen now.
    fn clear_view(&mut self) {
        self.clear_floor = self.raw.as_ref().map_or(0, |raw| raw.history.len());
        self.republish();
    }

    /// The next action for the driver, if any. Navigation steps (from taps)
    /// are applied here and never returned.
    pub fn try_action(&mut self) -> Option<UiAction> {
        // Ctrl+V with clipboard text: the text joins the composer.
        if let Some(text) = take_offered_text() {
            let combined = format!("{}{text}", draft_text());
            self.nav.set_text(&combined);
            self.notify();
        }
        self.expire_note();
        loop {
            match self.actions.try_recv().ok()? {
                UiAction::Nav(step) => self.nav_step(step),
                UiAction::Tools(step) => self.tools_step(step),
                UiAction::Clear => self.clear_view(),
                UiAction::Context => self.context_note(),
                UiAction::Exit => QUIT.store(true, Ordering::SeqCst),
                action if action.is_portal() => self.portal.push_back(action),
                action => return Some(action),
            }
        }
    }

    /// The next owner-portal action (web host), if any. Call after
    /// [`Host::try_action`], which moves them here.
    /// Whether the Tasks screen is on top: the terminal loop keeps waking for
    /// its refresh timer while it is.
    pub fn tasks_open(&self) -> bool {
        self.nav.on_tasks()
    }

    pub fn take_portal(&mut self) -> Option<UiAction> {
        self.portal.pop_front()
    }

    /// The screen on top (for the portal's timers and the web host).
    pub fn screen(&self) -> Screen {
        self.nav.top()
    }

    /// The session the page is attached to.
    pub fn session(&self) -> Option<Uuid> {
        self.nav.session()
    }

    /// Makes the portal's sign-in or list the app's screen (or Chat).
    pub fn set_root(&mut self, screen: Screen) {
        self.nav.set_root(screen);
        self.notify();
    }

    pub fn set_portal(&mut self, rows: &[PortalRow]) {
        self.nav.set_portal(rows);
        self.notify();
    }

    pub fn set_signin_error(&mut self, error: Option<String>) {
        self.nav.set_signin_error(error);
        self.notify();
    }

    pub fn set_share_enabled(&mut self, enabled: bool) {
        self.nav.set_share_enabled(enabled);
        self.notify();
    }

    pub fn set_invites(&mut self, invites: Vec<InviteLine>) {
        self.nav.set_invites(invites);
        self.notify();
    }

    pub fn set_created(&mut self, url: Option<String>) {
        self.nav.set_created(url);
        self.notify();
    }

    pub fn set_share_note(&mut self, note: Option<String>) {
        self.nav.set_share_note(note);
        self.notify();
    }

    /// Routes one terminal key. Returns `Some(input)` for the view to handle,
    /// or `None` when the key became an action or a screen step here.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn filter(&mut self, input: Input) -> Option<Input> {
        self.filter_at(input, Instant::now())
    }

    /// [`Host::filter`] at the injected time `now` (the type-ahead guard).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn filter_at(&mut self, input: Input, now: Instant) -> Option<Input> {
        let Input::Key { key, modifiers, release: false } = &input else {
            return Some(input);
        };
        if self.route_at(host_key(key), modifiers.alt, has_chord(modifiers), now) {
            None
        } else {
            Some(input)
        }
    }

    /// Routes a key by name: [`Host::route`] with [`host_key`].
    pub fn take_key(&mut self, key: &str, alt: bool, chord: bool) -> bool {
        self.route(host_key(key), alt, chord)
    }

    /// Whether a turn is running (the host's interrupt goes out only then).
    pub fn running(&self) -> bool {
        self.running
    }

    /// Aborts the running turn (the terminal's first Ctrl+C).
    pub fn interrupt(&mut self) {
        self.act(UiAction::Interrupt);
    }

    /// Routes one key, the same on every host. Returns true when the host
    /// consumes the key (an action, a screen step, or an ignored key); false
    /// when the composer gets it.
    pub fn route(&mut self, key: Key, alt: bool, chord: bool) -> bool {
        self.route_at(key, alt, chord, Instant::now())
    }

    /// [`Host::route`] at the injected time `now`.
    pub fn route_at(&mut self, key: Key, alt: bool, chord: bool, now: Instant) -> bool {
        let draft = draft_text();
        let items = suggest::for_draft(&draft);
        self.nav.set_slash(items.names.clone());
        let facts = KeyFacts {
            approval_options: self.pending,
            plan_pending: self.plan_pending,
            queued: self.queued,
            screen: self.nav.top(),
            filter_active: !self.nav.filter().is_empty(),
            selected_session: self.nav.selected_session(),
            form_open: self.nav.form_open(),
            selected_task: self.nav.selected_task(),
            enter_setting: self.nav.enter_setting(),
            removable: self.nav.settings_removable(),
            save_default: self.nav.save_default_action(),
            slash_len: if self.nav.suggestions_shown() { items.names.len() } else { 0 },
            file_mode: items.file,
            tool_panel: self.tool_panel.is_some(),
            slash_exact: slash::is_exact(&draft),
            selected_index: self.nav.selected(),
            selected_model: self.nav.selected_model(),
            first_agent: self.nav.first_agent(),
            agent_live: self.nav.agent_live(),
            confirming: self.nav.confirming(),
            running: self.running,
            recalling: history_pos().is_some(),
            history_len: HISTORY.lock().map(|h| h.len()).unwrap_or(0),
            auth_keying: self.nav.auth_keying(),
            attachments: staged_count(),
            guarded: prompt_guard_open(now),
            scrolled_up: scroll_offset() > 0,
        };
        let routed = route_key(key, alt, chord, facts, &draft);
        // Typing or deleting changes the text: the suggestions show again.
        if matches!(routed, Routed::Composer)
            && matches!(key, Key::Char(_) | Key::Backspace)
            && self.nav.reopen()
        {
            self.notify();
        }
        match routed {
            Routed::Composer => false,
            Routed::Recall { up } => {
                self.recall(up);
                true
            }
            Routed::Scroll(step) => {
                self.scroll(step);
                true
            }
            Routed::Action(action) => {
                arm_plan_clear(&action);
                if matches!(action, UiAction::Queue(_)) {
                    // The text moved to the queue: the composer empties.
                    self.nav.set_text("");
                    self.notify();
                }
                self.act(action);
                true
            }
            Routed::ClearDraft => {
                self.clear_draft();
                true
            }
            Routed::Nav(step) => {
                self.nav_step(step);
                true
            }
            Routed::Tools(step) => {
                self.tools_step(step);
                true
            }
            Routed::CompleteFile => {
                let selected = self.nav.view().slash_selected;
                if let Some(text) =
                    items.names.get(selected).and_then(|path| suggest::splice_file(&draft, path))
                {
                    self.nav.set_text(&text);
                    self.notify();
                }
                true
            }
            Routed::Ignored => true,
        }
    }

    /// Up / Down through the sent inputs. The live draft is kept while an
    /// entry is recalled and restored by Down past the newest entry.
    fn recall(&mut self, up: bool) {
        let history = HISTORY.lock().map(|h| h.clone()).unwrap_or_default();
        let next = match (history_pos(), up) {
            (None, true) if !history.is_empty() => {
                if let Ok(mut saved) = HISTORY_DRAFT.lock() {
                    *saved = draft_text();
                }
                history.len() - 1
            }
            (Some(at), true) => at.saturating_sub(1),
            (Some(at), false) if at + 1 < history.len() => at + 1,
            (Some(_), false) => {
                set_history_pos(None);
                let live = HISTORY_DRAFT.lock().map(|d| d.clone()).unwrap_or_default();
                self.nav.set_text(&live);
                self.notify();
                return;
            }
            _ => return,
        };
        set_history_pos(Some(next));
        self.nav.set_text(&history[next]);
        self.notify();
    }

    /// A wheel or touch movement over the transcript (web host). Returns the
    /// same offset change a PageUp or PageDown key makes, by rows.
    pub fn wheel(&mut self, delta_y: f64) {
        let rows = rows_for_pixels(delta_y);
        if rows == 0 {
            return;
        }
        // The wheel moves the help rows on the help screen (up is toward the top).
        if self.nav.top() == Screen::Help {
            self.nav_step(NavStep::HelpScroll(-rows));
        } else {
            self.scroll(ScrollStep::Lines(rows));
        }
    }

    /// Shows the theme in use at startup (the saved choice, not saved again).
    pub fn restore_theme(&mut self, choice: Choice) {
        self.nav.restore_theme(choice);
        self.notify();
    }

    /// PageUp, PageDown, Home and End: moves the transcript's offset from
    /// the newest row. The view scrolls to it once per step (SCROLL_SEQ).
    fn scroll(&mut self, step: ScrollStep) {
        let total = LINE_TOTAL.load(Ordering::SeqCst);
        let current = scroll_offset();
        let next = match step {
            ScrollStep::Up => current.saturating_add(SCROLL_PAGE).min(total.saturating_sub(1)),
            ScrollStep::Down => current.saturating_sub(SCROLL_PAGE),
            ScrollStep::Top => total.saturating_sub(1),
            ScrollStep::Bottom => 0,
            ScrollStep::Lines(rows) => {
                let moved = current as isize + rows;
                moved.clamp(0, total.saturating_sub(1) as isize) as usize
            }
        };
        set_scroll_offset(next);
        if next == 0 {
            NEW_BELOW.store(false, Ordering::SeqCst);
        }
        SCROLL_SEQ.fetch_add(1, Ordering::SeqCst);
        self.notify();
    }

    /// Applies a screen step now (so the next key sees its result), sends
    /// the core calls it implies, and tells the view.
    fn nav_step(&mut self, step: NavStep) {
        for action in self.nav.apply(step) {
            self.act(action);
        }
        self.notify();
    }

    /// The tool detail panel: Toggle opens it at the newest row (or sends the
    /// no-activity note), Close and a second Toggle close it, and the moves
    /// stay within the rows the snapshot holds.
    fn tools_step(&mut self, step: ToolsStep) {
        let last = self.tool_len.saturating_sub(TOOL_PANEL_SLOTS);
        self.tool_panel = match (step, self.tool_panel) {
            (ToolsStep::Toggle, Some(_)) | (ToolsStep::Close, _) => None,
            (ToolsStep::Toggle, None) if self.tool_len == 0 => {
                self.act(UiAction::Notice(NO_TOOLS_NOTE.to_string()));
                None
            }
            (ToolsStep::Toggle, None) => Some(last),
            (_, None) => None,
            (ToolsStep::Move(delta), Some(top)) => {
                Some((top as isize + delta).clamp(0, last as isize) as usize)
            }
            (ToolsStep::Top, Some(_)) => Some(0),
            (ToolsStep::Bottom, Some(_)) => Some(last),
        };
        self.notify();
    }

    fn notify(&self) {
        let mut view = self.nav.view();
        view.tool_panel = self.tool_panel;
        view.scroll_seq = SCROLL_SEQ.load(Ordering::SeqCst);
        view.scroll_offset = scroll_offset();
        view.new_below = NEW_BELOW.load(Ordering::SeqCst);
        let _ = self.navs.send(view);
    }

    fn act(&self, action: UiAction) {
        let _ = self.loopback.send(action);
    }
}

fn history_pos() -> Option<usize> {
    HISTORY_POS.lock().map(|p| *p).unwrap_or(None)
}

fn set_history_pos(pos: Option<usize>) {
    if let Ok(mut p) = HISTORY_POS.lock() {
        *p = pos;
    }
}

fn scroll_offset() -> usize {
    SCROLL_OFFSET.lock().map(|o| *o).unwrap_or(0)
}

fn set_scroll_offset(offset: usize) {
    if let Ok(mut o) = SCROLL_OFFSET.lock() {
        *o = offset;
    }
}

/// Records a sent input for Up / Down recall (the view calls this on submit)
/// and leaves recall, so the next Up starts from the newest entry.
pub(crate) fn record_sent(text: &str) {
    if let Ok(mut history) = HISTORY.lock() {
        history.push(text.to_string());
        let excess = history.len().saturating_sub(HISTORY_MAX);
        history.drain(..excess);
    }
    set_history_pos(None);
}

/// The view reports how many lines the transcript holds now.
pub(crate) fn set_line_total(lines: usize) {
    LINE_TOTAL.store(lines, Ordering::SeqCst);
}

/// Rows the transcript shows at its current height (the view reports it).
static VISIBLE_ROWS: AtomicUsize = AtomicUsize::new(0);

/// The lines above the view when it shows `visible` rows of `total`, ending
/// `offset` rows above the newest. The terminal's "N lines above" count.
pub fn lines_above(total: usize, offset: usize, visible: usize) -> usize {
    total.saturating_sub(offset + visible)
}

/// Clears the composer after a revise note went out, once the plan is no
/// longer pending. The view calls this for every snapshot it receives.
/// The animation step period, on the app clock. The old TUI polled every
/// 50 ms (crates/modron-tui/src/lib.rs:2446); the spinner and hints step at
/// this rate, and the hint holds for `IDLE_HINT_TICKS` steps.
pub const ANIM_PERIOD: std::time::Duration = std::time::Duration::from_millis(120);

/// Animation steps taken so far; the spinner frame and idle hint read it.
static ANIM_TICK: AtomicU64 = AtomicU64::new(0);
/// When the next animation step is due on the app clock.
static ANIM_NEXT: Mutex<Option<Instant>> = Mutex::new(None);
/// Whether the header shows an idle hint (so clearing it republishes once).
static HINT_SHOWN: AtomicBool = AtomicBool::new(false);

/// The header hint for a state: the interrupt hint while a turn runs, the
/// rotating idle hint while the composer is empty, nothing once the user has
/// typed.
/// How soon the terminal loop must wake without an event: the animation
/// period while a turn runs (the spinner advances on each wake), and no
/// deadline when idle, so an idle app sleeps until input.
pub fn tick_deadline(running: bool) -> Option<std::time::Duration> {
    running.then_some(ANIM_PERIOD)
}

pub fn hint_text(running: bool, composer_empty: bool, tick: u64) -> String {
    if running {
        RUNNING_HINT.to_string()
    } else if composer_empty {
        idle_hint(tick).to_string()
    } else {
        String::new()
    }
}

/// The spinner glyph: the frame while a turn runs, nothing when idle.
pub fn spinner_text(running: bool, tick: u64) -> String {
    if running {
        spinner_frame(tick).to_string()
    } else {
        String::new()
    }
}

pub fn after_snapshot(snapshot: &ViewModelSnapshot, draft: Signal<String>) {
    if snapshot.plan_pending.is_none() && CLEAR_DRAFT.swap(false, Ordering::SeqCst) {
        set_draft("");
        draft.set(String::new());
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn has_chord(m: &Modifiers) -> bool {
    m.ctrl || m.alt || m.cmd
}

/// A theme colour by its `theme/oculus.json` token name. Read reactively,
/// so a new theme reaches the text that reads it.
pub(crate) fn token_ink(name: &str) -> AdaptiveColor {
    tokens().color(name).unwrap_or_else(|| AdaptiveColor::from(nsh_core::color::black()))
}

/// The theme token each row style is drawn with.
pub(crate) fn style_token(style: Style) -> &'static str {
    match style {
        Style::Bold => "primary",
        Style::Plain => "foreground",
        Style::Meta => "muted-foreground",
        Style::Tool => "tool",
        Style::Dim => "dim",
    }
}

/// The colour token of a tool row, by its shared status mark: `✓` success,
/// `✗` destructive, anything else (`● name …`) the tool colour.
fn tool_row_token(line: &str) -> &'static str {
    if line.starts_with('\u{2713}') {
        "success"
    } else if line.starts_with('\u{2717}') {
        "destructive"
    } else {
        "tool"
    }
}

fn is_style(model: Signal<Model>, at: Position, want: Style) -> bool {
    model.with(|m| m.lines.get(at.index()).is_some_and(|l| l.style == want))
}

fn line_text(model: Signal<Model>, at: Position) -> String {
    model.with(|m| m.lines.get(at.index()).map(|l| l.text.clone()).unwrap_or_default())
}

/// One transcript line in its row style.
fn styled_line(model: Signal<Model>, at: Position, style: Style) -> impl IntoElement {
    let line = text(move || line_text(model, at)).text_color(move || token_ink(style_token(style)));
    match style {
        Style::Bold => line.font_weight(FontWeight::BOLD),
        Style::Meta => line.italic(),
        Style::Plain | Style::Tool | Style::Dim => line,
    }
}

/// One transcript cell. Cells are recycled, so each region reads the row
/// position reactively and shows only while its style matches.
fn transcript_row(model: Signal<Model>, at: Position) -> impl IntoElement {
    view()
        .child(when(
            move || is_style(model, at, Style::Bold),
            move || styled_line(model, at, Style::Bold),
        ))
        .child(when(
            move || is_style(model, at, Style::Meta),
            move || styled_line(model, at, Style::Meta),
        ))
        .child(when(
            move || is_style(model, at, Style::Plain),
            move || styled_line(model, at, Style::Plain),
        ))
        .child(when(
            move || is_style(model, at, Style::Tool),
            move || styled_line(model, at, Style::Tool),
        ))
        .child(when(
            move || is_style(model, at, Style::Dim),
            move || styled_line(model, at, Style::Dim),
        ))
}

pub(crate) fn header(model: Signal<Model>) -> impl IntoElement {
    view()
        .flex_row()
        .child(
            text(move || model.with(|m| m.connection.clone()))
                .text_color(move || token_ink("muted-foreground")),
        )
        .child(
            text(move || model.with(|m| m.provider.clone()))
                .text_color(move || token_ink("muted-foreground")),
        )
        .child(
            text(move || model.with(|m| m.running.clone()))
                .text_color(move || token_ink("muted-foreground")),
        )
        .child(
            text(move || model.with(|m| m.spinner.clone()))
                .text_color(move || token_ink("muted-foreground")),
        )
        .child(
            text(move || model.with(|m| m.hint.clone()))
                .text_color(move || token_ink("muted-foreground")),
        )
        .child(
            text(move || model.with(|m| m.session.clone()))
                .text_color(move || token_ink("muted-foreground")),
        )
        .child(
            text(move || model.with(|m| m.goal.clone()))
                .text_color(move || token_ink("muted-foreground")),
        )
}

/// Up to `max` rows of `rows(model)`, one text element per slot. A slot
/// shows only while its row exists, so the layout stays fixed. `token` picks
/// each row's colour token from its text.
fn row_slots(
    model: Signal<Model>,
    max: usize,
    rows: fn(&Model) -> Vec<String>,
    token: fn(&str) -> &'static str,
    italic: bool,
) -> impl IntoElement {
    let mut column = view().flex_col();
    for i in 0..max {
        column = column.child(when(
            move || model.with(|m| rows(m).len() > i),
            move || {
                let line =
                    text(move || model.with(|m| rows(m).get(i).cloned().unwrap_or_default()))
                        .text_color(move || {
                            token_ink(token(
                                &model.with(|m| rows(m).get(i).cloned().unwrap_or_default()),
                            ))
                        });
                if italic {
                    line.italic()
                } else {
                    line
                }
            },
        ));
    }
    column
}

fn plain_token(_: &str) -> &'static str {
    style_token(Style::Plain)
}

fn dim_token(_: &str) -> &'static str {
    style_token(Style::Meta)
}

fn tool_token(_: &str) -> &'static str {
    style_token(Style::Tool)
}

fn attachment_rows(m: &Model) -> Vec<String> {
    m.attachments.clone()
}

fn queue_rows(m: &Model) -> Vec<String> {
    m.queued
        .iter()
        .enumerate()
        .map(|(i, q)| modron_mobile_core::view::queue_chip(i + 1, q))
        .collect()
}

/// The queue chips above the composer. On the web each chip is a button
/// that removes its message (`Dequeue` at its position); the terminal shows
/// the chips as text and removes the last with Backspace.
fn queue_chips(model: Signal<Model>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    let mut column = view().flex_col();
    for i in 0..QUEUE_SLOTS {
        let actions = actions.clone();
        column = column.child(when(
            move || model.with(|m| m.queued.len() > i),
            move || {
                let label =
                    move || model.with(|m| queue_rows(m).get(i).cloned().unwrap_or_default());
                let chip = view()
                    .child(text(label).text_color(move || token_ink(style_token(Style::Plain))));
                if WEB_CONTROLS {
                    let actions = actions.clone();
                    chip.role(AccessibilityRole::Button).on_press(move || {
                        let _ = actions.send(UiAction::Dequeue(i));
                    })
                } else {
                    chip
                }
            },
        ));
    }
    column
}

fn steer_rows(m: &Model) -> Vec<String> {
    m.pending_steers.iter().map(|s| modron_mobile_core::view::steer_echo(s)).collect()
}

fn plan_line_rows(m: &Model) -> Vec<String> {
    m.plan.as_ref().map(|p| p.lines.clone()).unwrap_or_default()
}

/// The last `TOOL_SLOTS` tool rows: the live strip shows the latest activity.
fn tool_rows(m: &Model) -> Vec<String> {
    let start = m.tool_rows.len().saturating_sub(TOOL_SLOTS);
    m.tool_rows[start..].to_vec()
}

fn note_rows(m: &Model) -> Vec<String> {
    m.notes.clone()
}

/// The live tool strip: the turn summary line, then the latest tool rows.
/// Shown while the turn has tool activity; the committed `ToolSummary` row
/// takes over in the transcript once the turn ends. On the web the summary
/// line is a button that opens the tool detail panel.
fn tool_strip(model: Signal<Model>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    let line = text(move || model.with(|m| m.turn_summary.clone().unwrap_or_default()))
        .text_color(move || token_ink(tool_token("")));
    let head = if WEB_CONTROLS {
        view()
            .role(AccessibilityRole::Button)
            .on_press(move || {
                let _ = actions.send(UiAction::Tools(ToolsStep::Toggle));
            })
            .child(line)
    } else {
        view().child(line)
    };
    view().flex_col().child(head).child(row_slots(
        model,
        TOOL_SLOTS,
        tool_rows,
        tool_row_token,
        false,
    ))
}

/// The tool detail panel (Ctrl+T): the title, then up to TOOL_PANEL_SLOTS
/// rows of the snapshot's `tool_detail` from the panel's top row, verbatim.
fn tool_panel(model: Signal<Model>, nav: Signal<NavView>) -> impl IntoElement {
    let top = move || nav.with(|n| n.tool_panel.unwrap_or(0));
    let mut column = view().flex_col().child(
        text(move || model.with(|m| m.tool_title.clone()))
            .text_color(move || token_ink(tool_token(""))),
    );
    for i in 0..TOOL_PANEL_SLOTS {
        column = column.child(when(
            move || model.with(|m| m.tool_detail.len() > top() + i),
            move || {
                let row = move || {
                    model.with(|m| m.tool_detail.get(top() + i).cloned().unwrap_or_default())
                };
                text(row).text_color(move || token_ink(tool_row_token(&row())))
            },
        ));
    }
    column
}

/// `y approve   r revise`: the key hint and the core's label per option.
fn plan_option_row(labels: &[String]) -> String {
    labels
        .iter()
        .enumerate()
        .map(|(i, label)| format!("{} {label}", PLAN_KEYS.get(i).copied().unwrap_or("")))
        .collect::<Vec<_>>()
        .join("   ")
}

/// One option of a decision: a text line on the terminal (key hint and the
/// core's label), a tappable button on the web (the core's label only).
fn option_cell(
    label: impl Fn() -> String + 'static,
    token: &'static str,
    actions: UnboundedSender<UiAction>,
    action: impl Fn() -> UiAction + 'static,
) -> impl IntoElement {
    let cell = view().child(text(label).text_color(move || token_ink(token)));
    if WEB_CONTROLS {
        cell.role(AccessibilityRole::Button).px(px(8.0)).on_press(move || {
            // The same type-ahead window as the keys: a tap on a fresh
            // prompt (a double tap on send, say) decides nothing.
            if prompt_guard_open(Instant::now()) {
                return;
            }
            let action = action();
            arm_plan_clear(&action);
            let _ = actions.send(action);
        })
    } else {
        cell
    }
}

fn plan_block(model: Signal<Model>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    let option = move |i: usize| {
        let label = move || {
            model.with(|m| {
                let label =
                    m.plan.as_ref().and_then(|p| p.labels.get(i).cloned()).unwrap_or_default();
                if WEB_CONTROLS {
                    label
                } else {
                    plan_option_row(&[label])
                }
            })
        };
        let token = if i == 0 { style_token(Style::Bold) } else { style_token(Style::Plain) };
        let actions = actions.clone();
        option_cell(label, token, actions, move || plan_answer(i == 0))
    };
    let lines = view().flex_col().child(row_slots(
        model,
        PLAN_LINE_SLOTS,
        plan_line_rows,
        plain_token,
        false,
    ));
    let row = if WEB_CONTROLS {
        view().flex_row().child(option(0)).child(option(1))
    } else {
        view().child(text(move || {
            model.with(|m| m.plan.as_ref().map(|p| plan_option_row(&p.labels)).unwrap_or_default())
        }))
    };
    lines.child(row)
}

fn approval_strip(model: Signal<Model>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    let option = move |i: usize| {
        let label = move || {
            model.with(|m| {
                m.approval
                    .as_ref()
                    .and_then(|a| a.labels.get(i))
                    .map(|label| {
                        if WEB_CONTROLS {
                            label.clone()
                        } else {
                            format!("{} {label}", DECISION_KEYS[i])
                        }
                    })
                    .unwrap_or_default()
            })
        };
        // Deny is the third option: the destructive token marks it.
        let token = match i {
            0 => style_token(Style::Bold),
            2 => "destructive",
            _ => style_token(Style::Plain),
        };
        let actions = actions.clone();
        option_cell(label, token, actions, move || UiAction::Decide(i))
    };
    view()
        .flex_col()
        .child(text(move || {
            model.with(|m| m.approval.as_ref().map(|a| a.tool.clone()).unwrap_or_default())
        }))
        .child(text(move || {
            model.with(|m| m.approval.as_ref().map(|a| a.args_summary.clone()).unwrap_or_default())
        }))
        .child(view().flex_row().child(option(0)).child(option(1)).child(option(2)))
}

/// The web-only composer controls: Queue beside the composer (queues the
/// composer text, as Alt+Enter does), and Stop while a turn runs.
/// The attach control's label (web).
const ATTACH_LABEL: &str = "attach";

/// Opens the browser's file chooser (web). The terminal has no chooser: its
/// image path is Ctrl+V.
#[cfg(target_arch = "wasm32")]
fn open_file_picker(actions: UnboundedSender<UiAction>) {
    crate::web::open_file_picker(actions);
}

#[cfg(not(target_arch = "wasm32"))]
fn open_file_picker(_actions: UnboundedSender<UiAction>) {}

fn composer_controls(
    model: Signal<Model>,
    draft: Signal<String>,
    actions: UnboundedSender<UiAction>,
) -> impl IntoElement {
    let queue_actions = actions.clone();
    let queue = move || {
        let queue_actions = queue_actions.clone();
        view()
            .role(AccessibilityRole::Button)
            .px(px(8.0))
            .on_press(move || {
                let task = draft_text().trim().to_string();
                if task.is_empty() {
                    return;
                }
                set_draft("");
                draft.set(String::new());
                let _ = queue_actions.send(UiAction::Queue(task));
            })
            .child(text(QUEUE_LABEL).text_color(move || token_ink(style_token(Style::Plain))))
    };
    let attach_actions = actions.clone();
    let attach = move || {
        let attach_actions = attach_actions.clone();
        view()
            .role(AccessibilityRole::Button)
            .px(px(8.0))
            .on_press(move || open_file_picker(attach_actions.clone()))
            .child(text(ATTACH_LABEL).text_color(move || token_ink(style_token(Style::Plain))))
    };
    let stop = move || {
        let stop_actions = actions.clone();
        view()
            .role(AccessibilityRole::Button)
            .px(px(8.0))
            .on_press(move || {
                let _ = stop_actions.send(UiAction::Interrupt);
            })
            .child(text(STOP_LABEL).text_color(move || token_ink("warning")))
    };
    view()
        .flex_row()
        .child(when(move || WEB_CONTROLS, queue))
        .child(when(move || WEB_CONTROLS, attach))
        .child(when(move || WEB_CONTROLS && model.with(|m| m.is_running), stop))
}

/// The chat body: the transcript, the live strips, the decision blocks and
/// the composer. The status surface and navigation are drawn around it by
/// `screens::frame`. `actions` receives the composer's own actions (Send,
/// Queue, Compact) and the web controls' actions; decisions, interrupts and
/// plan answers from the keys come from the host's key routing.
pub fn screen(
    model: Signal<Model>,
    draft: Signal<String>,
    nav: Signal<NavView>,
    window_h: Signal<f32>,
    actions: UnboundedSender<UiAction>,
) -> impl IntoElement {
    let submit_actions = actions.clone();
    let list_ref = element_ref();
    // The last scroll step the view applied (SCROLL_SEQ).
    let applied_scroll = std::cell::Cell::new(0u64);
    effect(move || {
        let rows = transcript_px(
            window_h.get(),
            nav.with(|n| n.tool_panel.is_some()),
            draft.with(|d| d.matches('\n').count() + 1),
        ) / COMPOSER_LINE_H;
        VISIBLE_ROWS.store(rows.max(1.0) as usize, Ordering::SeqCst);
    });
    effect(move || {
        let lines = model.with(|m| m.lines.len());
        set_line_total(lines);
        let (seq, offset) = nav.with(|n| (n.scroll_seq, n.scroll_offset));
        if seq == applied_scroll.get() {
            return;
        }
        applied_scroll.set(seq);
        if offset == 0 {
            list_ref.scroll_to_end(false);
        } else {
            // The row `offset` lines above the newest sits at the bottom edge.
            let index = (lines.saturating_sub(1)).saturating_sub(offset);
            list_ref.scroll_to_index(index as u32, Percentage(1.0), Points(0.0), false);
        }
    });
    let composer = input()
        .auto_focus(true)
        // Enter sends and keeps focus, so the next line can be typed at once.
        // Shift+Enter and Ctrl+J add a line (NSH's multiline input), and a
        // pasted block lands whole in the composer.
        .multiline(true)
        .max_h(px(COMPOSER_MAX_LINES as f32 * COMPOSER_LINE_H))
        .submit_behavior(SubmitBehavior::Submit)
        .placeholder(COMPOSER_PLACEHOLDER)
        .value(move || draft.get())
        .on_change(move |s: &str| {
            set_draft(s);
            draft.set(s.to_string());
        })
        .on_submit(move |s: &str| {
            // The old TUI's line continuation: a line ending in a backslash
            // turns the backslash into a line break and does not submit.
            if let Some(head) = s.strip_suffix('\\') {
                let next = format!("{head}\n");
                set_draft(&next);
                draft.set(next);
                return;
            }
            let task = s.trim().to_string();
            set_draft("");
            draft.set(String::new());
            if nav.with(|n| n.screen == Screen::Agent) {
                // The agent screen's composer steers the open agent (while it runs).
                let agent =
                    nav.with(|n| n.agent.as_ref().filter(|a| a.running).map(|a| a.id.clone()));
                if let (Some(agent), false) = (agent, task.is_empty()) {
                    let _ = submit_actions.send(UiAction::SteerAgent { agent, text: task });
                }
                return;
            }
            if !task.is_empty() {
                record_sent(&task);
            }
            let action = submit_action(&task);
            if !action_is_empty(&action) {
                let _ = submit_actions.send(action);
            }
        });
    view()
        .flex_col()
        .size_full()
        .child(when(move || nav.with(|n| n.screen == Screen::Agent), {
            let actions = actions.clone();
            move || crate::screens::agent_panel(nav, actions.clone())
        }))
        .child(
            virtual_list(
                move || model.with(|m| m.lines.len()),
                move |at: Position| transcript_row(model, at),
            )
            .element_ref(&list_ref)
            // Opens at the newest row and stays there while it is at the end;
            // scrolled up, what is visible stays put as rows arrive.
            .initial_scroll_at_end()
            .follow_end(px(24.0))
            // An explicit height: a flex-grown virtual list gets zero rows
            // in NSH 0.5.1 (spikes/nsh-tui/RESULTS.md, section 4). On the
            // web it follows the window, so the composer and the Stop
            // control stay on screen (the page itself does not scroll).
            .h(move || {
                px(transcript_px(
                    window_h.get(),
                    nav.with(|n| n.tool_panel.is_some()),
                    draft.with(|d| d.matches('\n').count() + 1),
                ))
            }),
        )
        .child(when(
            move || nav.with(|n| n.scroll_offset > 0),
            move || {
                text(move || {
                    let offset = nav.with(|n| n.scroll_offset);
                    let total = model.with(|m| m.lines.len());
                    let visible = VISIBLE_ROWS.load(Ordering::SeqCst);
                    scrolled_up_hint(lines_above(total, offset, visible))
                })
                .text_color(move || token_ink("muted-foreground"))
            },
        ))
        .child(when(
            move || nav.with(|n| n.new_below && n.scroll_offset > 0),
            move || text(NEW_OUTPUT_BELOW_LABEL).text_color(move || token_ink("warning")),
        ))
        .child(when(move || nav.with(|n| n.tool_panel.is_some()), move || tool_panel(model, nav)))
        .child(when(move || model.with(|m| m.turn_summary.is_some()), {
            let actions = actions.clone();
            move || tool_strip(model, actions.clone())
        }))
        .child(swarm_slots(model, nav, actions.clone()))
        .child(row_slots(model, ATTACH_SLOTS, attachment_rows, dim_token, true))
        .child(row_slots(model, NOTE_SLOTS, note_rows, dim_token, true))
        .child(row_slots(model, STEER_SLOTS, steer_rows, dim_token, true))
        .child(when(
            move || model.with(|m| !m.reasoning.is_empty()),
            move || {
                text(move || model.with(|m| m.reasoning.clone()))
                    .text_color(move || token_ink("muted-foreground"))
            },
        ))
        .child(when(
            move || model.with(|m| !m.streaming.is_empty()),
            move || {
                text(move || model.with(|m| m.streaming.clone()))
                    .text_color(move || token_ink("foreground"))
            },
        ))
        .child(when(move || model.with(|m| m.approval.is_some()), {
            let actions = actions.clone();
            move || approval_strip(model, actions.clone())
        }))
        .child(when(move || model.with(|m| m.plan.is_some()), {
            let actions = actions.clone();
            move || plan_block(model, actions.clone())
        }))
        .child(slash_list(draft, nav, actions.clone()))
        .child(queue_chips(model, actions.clone()))
        .child(composer)
        .child(composer_controls(model, draft, actions))
}

/// The live sub-agent rows. On the web each row is a button that opens its
/// agent screen; the terminal opens the first agent with Ctrl+A.
fn swarm_slots(
    model: Signal<Model>,
    nav: Signal<NavView>,
    actions: UnboundedSender<UiAction>,
) -> impl IntoElement {
    let mut column = view().flex_col();
    for i in 0..SWARM_SLOTS {
        let actions = actions.clone();
        column = column.child(when(
            move || model.with(|m| m.swarm_rows.len() > i),
            move || {
                let press = actions.clone();
                let line =
                    text(move || model.with(|m| m.swarm_rows.get(i).cloned().unwrap_or_default()))
                        .text_color(move || token_ink(tool_token("")));
                let row = view().child(line);
                if WEB_CONTROLS {
                    row.role(AccessibilityRole::Button).on_press(move || {
                        if let Some(id) = nav.with(|n| n.agents.get(i).map(|a| a.id.clone())) {
                            let _ = press.send(UiAction::Nav(NavStep::OpenAgent(id)));
                        }
                    })
                } else {
                    row
                }
            },
        ));
    }
    column
}

/// The suggestions above the composer: command suggestions (`usage - about`)
/// or `@file` paths, the selected one marked with `> `, a window of
/// SLASH_SLOTS rows around the selection. Esc hides them. On the web a tap
/// completes a command.
fn slash_list(
    draft: Signal<String>,
    nav: Signal<NavView>,
    actions: UnboundedSender<UiAction>,
) -> impl IntoElement {
    let items = move || suggest::for_draft(&draft.get());
    let start = move || {
        let len = items().names.len();
        let selected = nav.with(|n| n.slash_selected);
        if len <= SLASH_SLOTS {
            0
        } else {
            selected.saturating_sub(SLASH_SLOTS - 1).min(len - SLASH_SLOTS)
        }
    };
    let mut column = view().flex_col();
    for i in 0..SLASH_SLOTS {
        let actions = actions.clone();
        column = column.child(when(
            move || !nav.with(|n| n.slash_hidden) && items().names.len().min(SLASH_SLOTS) > i,
            move || {
                let press = actions.clone();
                let label = move || {
                    let at = start() + i;
                    let marker = if nav.with(|n| n.slash_selected) == at { "> " } else { "  " };
                    items().rows.get(at).map(|r| format!("{marker}{r}")).unwrap_or_default()
                };
                let row = view()
                    .child(text(label).text_color(move || token_ink(style_token(Style::Plain))));
                if WEB_CONTROLS {
                    row.role(AccessibilityRole::Button).on_press(move || {
                        if let Some(name) = items().names.get(start() + i).cloned() {
                            let _ = press.send(UiAction::Nav(NavStep::Complete(name)));
                        }
                    })
                } else {
                    row
                }
            },
        ));
    }
    column
}

/// An empty send is dropped; every other action goes to the driver.
fn action_is_empty(action: &UiAction) -> bool {
    matches!(action, UiAction::Send(t) | UiAction::Queue(t) if t.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nav::Screen;

    /// Serializes the tests that touch this module's process-global state (the
    /// chat statics, the prompt guard, the clipboard offer, `link()`). Each such
    /// test holds it for its whole body. Poison is recovered: a panicking test
    /// must not fail the rest.
    pub(crate) static TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    const NONE: KeyFacts = KeyFacts {
        removable: false,
        approval_options: None,
        plan_pending: false,
        queued: 0,
        screen: Screen::Chat,
        filter_active: false,
        selected_session: None,
        enter_setting: None,
        save_default: None,
        attachments: 0,
        slash_len: 0,
        slash_exact: false,
        selected_index: 0,
        selected_model: None,
        first_agent: None,
        agent_live: false,
        confirming: false,
        running: false,
        recalling: false,
        history_len: 0,
        tool_panel: false,
        file_mode: false,
        auth_keying: false,
        form_open: false,
        selected_task: None,
        guarded: false,
        scrolled_up: false,
    };

    fn approval(options: usize) -> KeyFacts {
        KeyFacts { approval_options: Some(options), ..NONE }
    }

    fn plan() -> KeyFacts {
        KeyFacts { plan_pending: true, ..NONE }
    }

    fn queued(count: usize) -> KeyFacts {
        KeyFacts { queued: count, ..NONE }
    }

    fn composer() -> Routed {
        Routed::Composer
    }

    #[test]
    fn key_names_map_to_keys() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(key_of("Enter"), Key::Enter);
        assert_eq!(key_of("Escape"), Key::Escape);
        assert_eq!(key_of("y"), Key::Char('y'));
        assert_eq!(key_of(" "), Key::Char(' '));
        assert_eq!(key_of("Backspace"), Key::Backspace);
        assert_eq!(key_of("ArrowUp"), Key::Other);
        assert_eq!(key_of("ArrowLeft"), Key::Other);
        assert_eq!(key_of(""), Key::Other);
    }

    #[test]
    fn esc_returns_a_scrolled_transcript_to_the_tail_after_everything_else() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let scrolled = KeyFacts { scrolled_up: true, ..NONE };
        assert_eq!(
            route_key(Key::Escape, false, false, scrolled.clone(), ""),
            Routed::Scroll(ScrollStep::Bottom)
        );
        // Precedence: a non-empty draft clears first, then the tail.
        assert_eq!(
            route_key(Key::Escape, false, false, scrolled.clone(), "typed"),
            Routed::ClearDraft
        );
        // A running turn is interrupted before the tail is reached.
        let running_scrolled = KeyFacts { running: true, ..scrolled.clone() };
        assert_eq!(
            route_key(Key::Escape, false, false, running_scrolled, ""),
            Routed::Action(UiAction::Interrupt)
        );
        // A pending approval is denied first.
        let pending = KeyFacts { approval_options: Some(3), ..scrolled.clone() };
        assert_eq!(
            route_key(Key::Escape, false, false, pending, ""),
            Routed::Action(UiAction::Decide(2))
        );
        // Open suggestions close first.
        let suggesting = KeyFacts { slash_len: 4, ..scrolled };
        assert_eq!(
            route_key(Key::Escape, false, false, suggesting, "/"),
            Routed::Nav(NavStep::SlashHide)
        );
        // Idle, live, empty: nothing.
        assert_eq!(route_key(Key::Escape, false, false, NONE, ""), Routed::Ignored);
    }

    #[test]
    fn lines_above_is_what_the_view_does_not_show() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        // 100 rows, 20 visible, at the tail: none above.
        assert_eq!(lines_above(100, 0, 20), 80);
        // Scrolled 30 rows up: 50 rows sit above the 20 shown.
        assert_eq!(lines_above(100, 30, 20), 50);
        assert_eq!(lines_above(10, 0, 20), 0);
    }

    fn guarded(facts: KeyFacts) -> KeyFacts {
        KeyFacts { guarded: true, ..facts }
    }

    #[test]
    fn a_fresh_prompt_ignores_its_decision_keys_and_passes_them_nowhere() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        // Inside the window, y / a / n and Esc (deny) are consumed: Ignored,
        // never Composer and never an action.
        for c in ['y', 'a', 'n'] {
            assert_eq!(
                route_key(Key::Char(c), false, false, guarded(approval(3)), ""),
                Routed::Ignored,
                "{c}"
            );
        }
        assert_eq!(route_key(Key::Escape, false, false, guarded(approval(3)), ""), Routed::Ignored);
        assert_eq!(route_key(Key::Char('r'), false, false, guarded(plan()), ""), Routed::Ignored);
        assert_eq!(route_key(Key::Char('y'), false, false, guarded(plan()), ""), Routed::Ignored);
        // Keys that are not decisions still reach the composer or their own use.
        assert_eq!(route_key(Key::Char('x'), false, false, guarded(approval(3)), ""), composer());
        assert_eq!(route_key(Key::Enter, false, false, guarded(approval(3)), "x"), composer());
        assert_eq!(route_key(Key::Char('r'), false, false, guarded(approval(3)), ""), composer());
        assert_eq!(route_key(Key::Char('n'), false, false, guarded(plan()), ""), composer());
        // A chord is not a decision.
        assert_eq!(route_key(Key::Char('a'), false, true, guarded(approval(3)), ""), composer());
    }

    #[test]
    fn once_the_window_is_closed_the_same_keys_decide() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            route_key(Key::Char('y'), false, false, approval(3), ""),
            Routed::Action(UiAction::Decide(0))
        );
        assert_eq!(
            route_key(Key::Escape, false, false, approval(3), ""),
            Routed::Action(UiAction::Decide(2))
        );
        assert_eq!(
            route_key(Key::Char('y'), false, false, plan(), ""),
            Routed::Action(UiAction::DecidePlan { approve: true, note: None })
        );
    }

    #[test]
    fn the_guard_is_about_prompts_not_about_the_screen() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        // Off the chat screen the guard does not apply (no approval there).
        let settings = KeyFacts { screen: Screen::Settings, ..NONE };
        for key in [Key::Char('y'), Key::Escape] {
            assert_eq!(
                route_key(key, false, false, guarded(settings.clone()), ""),
                route_key(key, false, false, settings.clone(), ""),
                "{key:?}"
            );
        }
    }

    #[test]
    fn letters_reach_the_composer_when_nothing_is_pending() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        for c in ['y', 'a', 'n', 'r', 'x', ' '] {
            assert_eq!(route_key(Key::Char(c), false, false, NONE, ""), composer(), "{c}");
        }
        assert_eq!(route_key(Key::Other, false, false, plan(), ""), composer());
    }

    #[test]
    fn transcript_leaves_the_controls_on_a_short_window() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        // At 720 px the transcript is 390 px, so the composer and the
        // approval and Stop rows fit below it (e2e S4 and S7d).
        assert_eq!(fit_transcript_height(720.0), 390.0);
        assert!(fit_transcript_height(720.0) + CHROME_RESERVE_H <= 720.0);
        assert_eq!(fit_transcript_height(1200.0), TRANSCRIPT_MAX_H);
        // The open tool panel and a growing composer (up to its cap) give the
        // transcript less room; the composer's cap holds the growth.
        assert_eq!(transcript_px(930.0, false, 1), fit_transcript_height(930.0));
        assert_eq!(transcript_px(930.0, true, 1), fit_transcript_height(930.0 - TOOL_PANEL_H));
        assert_eq!(
            transcript_px(930.0, false, 40),
            transcript_px(930.0, false, COMPOSER_MAX_LINES),
            "the composer stops growing at its cap"
        );
        assert_eq!(fit_transcript_height(100.0), TRANSCRIPT_MIN_H);
    }

    fn running() -> KeyFacts {
        KeyFacts { running: true, ..NONE }
    }

    #[test]
    fn escape_with_an_approval_denies_it_before_anything_else() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        // Deny is the third decision ("n"), the last option in protocol order.
        let deny = Routed::Action(UiAction::Decide(2));
        assert_eq!(route_key(Key::Escape, false, false, approval(3), ""), deny);
        assert_eq!(
            route_key(Key::Escape, false, false, KeyFacts { running: true, ..approval(3) }, ""),
            deny
        );
        let two = route_key(Key::Escape, false, false, approval(2), "");
        assert_eq!(two, Routed::Action(UiAction::Decide(1)));
        let no_options = route_key(Key::Escape, false, false, approval(0), "");
        assert_eq!(no_options, Routed::Ignored);
    }

    #[test]
    fn escape_with_a_plan_pending_does_nothing_destructive() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(route_key(Key::Escape, false, false, plan(), ""), Routed::Ignored);
        let both = KeyFacts { running: true, slash_len: 2, ..plan() };
        assert_eq!(route_key(Key::Escape, false, false, both, "/"), Routed::Ignored);
    }

    #[test]
    fn escape_closes_open_suggestions_before_it_interrupts() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let open = KeyFacts { slash_len: 2, ..running() };
        assert_eq!(
            route_key(Key::Escape, false, false, open, "/"),
            Routed::Nav(NavStep::SlashHide)
        );
    }

    #[test]
    fn escape_interrupts_a_running_turn_and_does_nothing_idle() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            route_key(Key::Escape, false, false, running(), ""),
            Routed::Action(UiAction::Interrupt)
        );
        assert_eq!(route_key(Key::Escape, false, false, NONE, ""), Routed::Ignored);
    }

    #[test]
    fn alt_enter_queues_the_draft_and_empty_or_command_drafts_do_not() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            route_key(Key::Enter, true, true, NONE, "  next task  "),
            Routed::Action(UiAction::Queue("next task".into()))
        );
        assert_eq!(route_key(Key::Enter, true, true, NONE, "   "), Routed::Ignored);
        assert_eq!(route_key(Key::Enter, true, true, NONE, "/help"), Routed::Ignored);
    }

    #[test]
    fn enter_reaches_the_composer_so_shift_enter_can_add_a_line() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(route_key(Key::Enter, false, false, approval(3), "x"), Routed::Composer);
        assert_eq!(route_key(Key::Enter, false, false, NONE, "a\nb"), Routed::Composer);
    }

    #[test]
    fn up_and_down_recall_only_on_an_empty_or_recalled_single_line() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let saved = KeyFacts { history_len: 2, ..NONE };
        assert_eq!(
            route_key(Key::Up, false, false, saved.clone(), ""),
            Routed::Recall { up: true }
        );
        // Empty history: nothing to recall.
        assert_eq!(route_key(Key::Up, false, false, NONE, ""), Routed::Ignored);
        // A typed draft is not replaced.
        assert_eq!(route_key(Key::Up, false, false, saved.clone(), "typing"), composer());
        // A recalled entry keeps the arrows, and Down returns to the draft.
        let recalled = KeyFacts { recalling: true, history_len: 2, ..NONE };
        assert_eq!(
            route_key(Key::Up, false, false, recalled.clone(), "old"),
            Routed::Recall { up: true }
        );
        assert_eq!(
            route_key(Key::Down, false, false, recalled.clone(), "edited"),
            Routed::Recall { up: false }
        );
        // Multi-line input moves between its lines.
        assert_eq!(route_key(Key::Up, false, false, saved.clone(), "a\nb"), composer());
        assert_eq!(route_key(Key::Down, false, false, recalled, "a\nb"), composer());
        // Suggestions keep the arrows while they show.
        let suggest = KeyFacts { slash_len: 2, ..saved };
        assert_eq!(
            route_key(Key::Up, false, false, suggest, "/"),
            Routed::Nav(NavStep::SlashMove(-1))
        );
    }

    #[test]
    fn page_keys_scroll_and_home_end_scroll_only_on_an_empty_composer() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(route_key(Key::PageUp, false, false, NONE, "x"), Routed::Scroll(ScrollStep::Up));
        assert_eq!(
            route_key(Key::PageDown, false, false, NONE, ""),
            Routed::Scroll(ScrollStep::Down)
        );
        assert_eq!(route_key(Key::Home, false, false, NONE, ""), Routed::Scroll(ScrollStep::Top));
        assert_eq!(route_key(Key::End, false, false, NONE, ""), Routed::Scroll(ScrollStep::Bottom));
        // With text, Home and End move the caret.
        assert_eq!(route_key(Key::Home, false, false, NONE, "x"), composer());
        assert_eq!(route_key(Key::End, false, false, NONE, "x"), composer());
        // A chord passes to the composer.
        assert_eq!(route_key(Key::PageUp, false, true, NONE, ""), composer());
    }

    #[test]
    fn shift_tab_flips_the_approval_mode_and_caret_keys_stay_in_the_composer() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            route_key(Key::BackTab, false, false, NONE, ""),
            Routed::Action(UiAction::CycleMode)
        );
        for key in [Key::Left, Key::Right, Key::Delete] {
            assert_eq!(route_key(key, false, false, NONE, "x"), composer());
        }
    }

    #[test]
    fn chords_pass_to_the_composer() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(route_key(Key::Char('a'), false, true, approval(3), ""), composer());
        assert_eq!(route_key(Key::Escape, false, true, NONE, ""), composer());
    }

    #[test]
    fn approval_keys_decide_their_options_in_order() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            route_key(Key::Char('y'), false, false, approval(3), ""),
            Routed::Action(UiAction::Decide(0))
        );
        assert_eq!(
            route_key(Key::Char('a'), false, false, approval(3), ""),
            Routed::Action(UiAction::Decide(1))
        );
        assert_eq!(
            route_key(Key::Char('n'), false, false, approval(3), ""),
            Routed::Action(UiAction::Decide(2))
        );
    }

    #[test]
    fn an_approval_key_beyond_the_option_count_is_the_composers() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(route_key(Key::Char('n'), false, false, approval(2), ""), composer());
    }

    #[test]
    fn an_approval_takes_precedence_over_a_plan() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let both = KeyFacts { approval_options: Some(3), plan_pending: true, ..NONE };
        assert_eq!(
            route_key(Key::Char('y'), false, false, both.clone(), ""),
            Routed::Action(UiAction::Decide(0))
        );
        assert_eq!(route_key(Key::Char('r'), false, false, both.clone(), "note"), composer());
    }

    #[test]
    fn plan_keys_approve_and_revise_with_the_draft_as_note() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            route_key(Key::Char('y'), false, false, plan(), "ignored"),
            Routed::Action(UiAction::DecidePlan { approve: true, note: None })
        );
        assert_eq!(
            route_key(Key::Char('r'), false, false, plan(), "  split the crate  "),
            Routed::Action(UiAction::DecidePlan {
                approve: false,
                note: Some("split the crate".into())
            })
        );
    }

    #[test]
    fn a_revise_with_an_empty_draft_has_no_note() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            route_key(Key::Char('r'), false, false, plan(), "   "),
            Routed::Action(UiAction::DecidePlan { approve: false, note: None })
        );
    }

    #[test]
    fn plan_pending_leaves_other_letters_to_the_composer() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(route_key(Key::Char('a'), false, false, plan(), ""), composer());
        assert_eq!(route_key(Key::Char('n'), false, false, plan(), ""), composer());
    }

    #[test]
    fn the_cursor_and_delete_keys_reach_the_composer_and_never_scroll_or_recall() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        for draft in ["", "one line", "a\nb"] {
            for key in [Key::Left, Key::Right, Key::Delete] {
                let facts = KeyFacts { history_len: 3, ..NONE };
                assert_eq!(
                    route_key(key, false, false, facts, draft),
                    composer(),
                    "{key:?} {draft:?}"
                );
            }
        }
    }

    #[test]
    fn up_and_down_on_a_multi_line_draft_reach_the_composer_not_recall() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        for key in [Key::Up, Key::Down] {
            let facts = KeyFacts { history_len: 3, ..NONE };
            assert_eq!(route_key(key, false, false, facts, "a\nb"), composer(), "{key:?}");
        }
    }

    #[test]
    fn the_reasoning_tail_is_the_last_two_lines_and_only_when_opted_in() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let mut s = ViewModelSnapshot {
            streaming_reasoning: "one\ntwo\nthree".into(),
            ..Default::default()
        };
        assert_eq!(Model::from_snapshot(&s).reasoning, "", "off by default");
        s.show_reasoning = true;
        assert_eq!(Model::from_snapshot(&s).reasoning, "two\nthree");
    }

    #[test]
    fn the_header_shows_the_first_eight_characters_of_the_session() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let id = uuid::Uuid::from_u128(0x1234_abcd_0000_4000_8000_0000_0000_0000);
        let s = ViewModelSnapshot { session_id: Some(id), ..Default::default() };
        assert_eq!(Model::from_snapshot(&s).session, "1234abcd");
        assert_eq!(Model::from_snapshot(&ViewModelSnapshot::default()).session, "");
    }

    #[test]
    fn a_blank_idle_session_shows_the_welcome_banner() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let welcome = modron_mobile_core::view::welcome_lines("m1", env!("CARGO_PKG_VERSION"));
        let blank = ViewModelSnapshot { model: "m1".into(), ..Default::default() };
        assert_eq!(Model::from_snapshot(&blank).lines.len(), welcome.len());
        let running = ViewModelSnapshot { model: "m1".into(), running: true, ..Default::default() };
        assert!(
            Model::from_snapshot(&running).lines.is_empty(),
            "a running blank session shows nothing"
        );
    }

    #[test]
    fn the_header_hint_and_spinner_follow_the_running_state_only() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(hint_text(true, false, 0), RUNNING_HINT);
        assert_eq!(hint_text(false, true, 8), idle_hint(8));
        assert_eq!(hint_text(false, false, 8), "", "typing hides the idle hint");
        assert_eq!(spinner_text(false, 3), "", "the spinner stops when idle");
        assert_eq!(spinner_text(true, 1), spinner_frame(1));
    }

    #[test]
    fn backspace_on_an_empty_composer_dequeues_the_last_message() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            route_key(Key::Backspace, false, false, queued(3), ""),
            Routed::Action(UiAction::Dequeue(2))
        );
        assert_eq!(
            route_key(Key::Backspace, false, false, queued(1), ""),
            Routed::Action(UiAction::Dequeue(0))
        );
    }

    #[test]
    fn backspace_does_nothing_with_an_empty_queue() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(route_key(Key::Backspace, false, false, NONE, ""), composer());
    }

    #[test]
    fn backspace_with_composer_text_edits_the_text() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(route_key(Key::Backspace, false, false, queued(2), "fix"), composer());
    }

    #[test]
    fn backspace_with_a_chord_is_the_composers() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(route_key(Key::Backspace, false, true, queued(2), ""), composer());
    }

    #[test]
    fn backspace_dequeues_while_an_approval_waits() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let both = KeyFacts { approval_options: Some(3), queued: 2, ..NONE };
        assert_eq!(
            route_key(Key::Backspace, false, false, both.clone(), ""),
            Routed::Action(UiAction::Dequeue(1))
        );
    }

    #[test]
    fn plan_answer_for_matches_the_key_rule() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            plan_answer_for(true, "note"),
            UiAction::DecidePlan { approve: true, note: None }
        );
        assert_eq!(
            plan_answer_for(false, " fix "),
            UiAction::DecidePlan { approve: false, note: Some("fix".into()) }
        );
    }

    fn sessions(filter_active: bool) -> KeyFacts {
        KeyFacts {
            screen: Screen::Sessions,
            filter_active,
            selected_session: Some(uuid::Uuid::from_u128(7)),
            ..NONE
        }
    }

    fn settings(enter: Option<UiAction>) -> KeyFacts {
        KeyFacts { screen: Screen::Settings, enter_setting: enter, ..NONE }
    }

    #[test]
    fn f2_opens_settings_and_ctrl_o_opens_sessions_from_any_screen() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        for facts in [NONE, sessions(false), settings(None), approval(3)] {
            assert_eq!(
                route_key(Key::F2, false, false, facts.clone(), ""),
                Routed::Nav(NavStep::Open(Screen::Settings))
            );
            assert_eq!(
                route_key(Key::Char('o'), false, true, facts, ""),
                Routed::Nav(NavStep::Open(Screen::Sessions))
            );
        }
    }

    #[test]
    fn host_key_maps_arrows_and_f2_and_leaves_other_names_alone() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(host_key("ArrowUp"), Key::Up);
        assert_eq!(host_key("ArrowDown"), Key::Down);
        assert_eq!(host_key("F2"), Key::F2);
        assert_eq!(host_key("Enter"), Key::Enter);
        assert_eq!(host_key("ArrowLeft"), Key::Left);
        assert_eq!(host_key("PageDown"), Key::PageDown);
        assert_eq!(host_key("BackTab"), Key::BackTab);
        assert_eq!(host_key("F1"), Key::F1);
        assert_eq!(host_key("F5"), Key::Other);
    }

    #[test]
    fn esc_clears_a_filter_before_it_closes_sessions() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            route_key(Key::Escape, false, false, sessions(true), ""),
            Routed::Nav(NavStep::ClearFilter)
        );
        assert_eq!(
            route_key(Key::Escape, false, false, sessions(false), ""),
            Routed::Nav(NavStep::Back)
        );
    }

    #[test]
    fn esc_closes_settings_and_interrupts_only_on_chat() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            route_key(Key::Escape, false, false, settings(None), ""),
            Routed::Nav(NavStep::Back)
        );
        assert_eq!(
            route_key(Key::Escape, false, false, running(), ""),
            Routed::Action(UiAction::Interrupt)
        );
    }

    #[test]
    fn sessions_typing_filters_and_n_or_jk_act_only_on_an_empty_filter() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            route_key(Key::Char('n'), false, false, sessions(false), ""),
            Routed::Nav(NavStep::NewSession)
        );
        assert_eq!(
            route_key(Key::Char('n'), false, false, sessions(true), ""),
            Routed::Nav(NavStep::Filter('n'))
        );
        assert_eq!(
            route_key(Key::Char('j'), false, false, sessions(false), ""),
            Routed::Nav(NavStep::Move(1))
        );
        assert_eq!(
            route_key(Key::Char('k'), false, false, sessions(false), ""),
            Routed::Nav(NavStep::Move(-1))
        );
        assert_eq!(
            route_key(Key::Char('j'), false, false, sessions(true), ""),
            Routed::Nav(NavStep::Filter('j'))
        );
        assert_eq!(
            route_key(Key::Char('x'), false, false, sessions(false), ""),
            Routed::Nav(NavStep::Filter('x'))
        );
    }

    #[test]
    fn enter_on_a_session_attaches_the_selected_row() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            route_key(Key::Enter, false, false, sessions(false), ""),
            Routed::Nav(NavStep::Attach(uuid::Uuid::from_u128(7)))
        );
        let empty = KeyFacts { screen: Screen::Sessions, ..NONE };
        assert_eq!(route_key(Key::Enter, false, false, empty, ""), Routed::Ignored);
    }

    #[test]
    fn enter_on_settings_sends_the_row_action_or_nothing() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let effort = UiAction::SetEffort(Some(Effort::Low));
        assert_eq!(
            route_key(Key::Enter, false, false, settings(Some(effort.clone())), ""),
            Routed::Action(effort)
        );
        assert_eq!(route_key(Key::Enter, false, false, settings(None), ""), Routed::Ignored);
    }

    #[test]
    fn arrows_move_on_sessions_and_settings() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        for facts in [sessions(false), settings(None)] {
            assert_eq!(
                route_key(Key::Up, false, false, facts.clone(), ""),
                Routed::Nav(NavStep::Move(-1))
            );
            assert_eq!(
                route_key(Key::Down, false, false, facts, ""),
                Routed::Nav(NavStep::Move(1))
            );
        }
    }

    #[test]
    fn backspace_edits_the_sessions_filter_and_is_a_no_op_on_settings() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            route_key(Key::Backspace, false, false, sessions(true), ""),
            Routed::Nav(NavStep::FilterBack)
        );
        assert_eq!(
            route_key(Key::Backspace, false, false, settings(None), ""),
            Routed::Nav(NavStep::FilterBack)
        );
    }

    #[test]
    fn letters_on_settings_are_consumed_not_typed() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(route_key(Key::Char('a'), false, false, settings(None), ""), Routed::Ignored);
    }

    #[test]
    fn a_chord_passes_to_the_composer_on_every_screen() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(route_key(Key::Char('a'), false, true, sessions(false), ""), Routed::Composer);
        assert_eq!(route_key(Key::Escape, false, true, settings(None), ""), Routed::Composer);
    }

    #[test]
    fn chat_keys_are_unchanged_by_the_new_screens() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(route_key(Key::Up, false, false, NONE, ""), Routed::Ignored);
        assert_eq!(route_key(Key::Char('n'), false, false, NONE, ""), Routed::Composer);
    }
}

#[cfg(test)]
mod auth_key_tests {
    use super::*;

    fn auth(keying: bool) -> KeyFacts {
        KeyFacts { screen: Screen::Auth, auth_keying: keying, ..Default::default() }
    }

    #[test]
    fn enter_activates_a_row_and_esc_backs_out_unless_a_key_is_being_typed() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            route_key(Key::Enter, false, false, auth(false), ""),
            Routed::Nav(NavStep::Activate)
        );
        assert_eq!(
            route_key(Key::Escape, false, false, auth(false), ""),
            Routed::Nav(NavStep::Back)
        );
        assert_eq!(
            route_key(Key::Escape, false, false, auth(true), ""),
            Routed::Nav(NavStep::AuthCancelKey)
        );
    }

    #[test]
    fn while_a_key_is_typed_its_keys_go_to_the_masked_input() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        for key in [Key::Char('s'), Key::Enter, Key::Backspace, Key::Up] {
            assert_eq!(route_key(key, false, false, auth(true), ""), Routed::Composer, "{key:?}");
        }
    }

    #[test]
    fn the_secret_key_never_prints() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let secret = SecretKey { provider: "openai".into(), key: "sk-live-1234".into() };
        let shown = format!("{secret:?}");
        assert!(shown.contains("openai"));
        assert!(!shown.contains("sk-live"), "{shown}");
        let action = UiAction::SetKey(secret);
        assert!(!format!("{action:?}").contains("sk-live"));
    }
}

#[cfg(test)]
mod first_run_tests {
    use super::*;
    use modron_mobile_core::view::AUTH_NOT_CONFIGURED;
    use modron_mobile_core::ViewModelSnapshot;
    use modron_protocol::AuthProviderWire;

    /// The first-run screen opens over Chat on the first snapshot with rows,
    /// and Esc closes it on the terminal host's route (no connection needed).
    #[test]
    fn esc_closes_the_first_run_credentials_screen() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (mut host, _client) = link();
        let snapshot = ViewModelSnapshot {
            providers: vec![AuthProviderWire {
                id: "openai".into(),
                label: "OpenAI".into(),
                status: AUTH_NOT_CONFIGURED.into(),
                oauth_capable: false,
            }],
            // The session's provider comes with its snapshot (Nav::first_run).
            provider: "openai".into(),
            ..Default::default()
        };
        host.push(&snapshot);
        assert_eq!(host.screen(), Screen::Auth);
        host.route(Key::Escape, false, false);
        assert_eq!(host.screen(), Screen::Chat);
    }
}

#[cfg(test)]
mod wheel_tests {
    use super::*;

    #[test]
    fn wheel_pixels_map_to_rows_toward_the_older_end() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(rows_for_pixels(0.0), 0);
        assert_eq!(rows_for_pixels(f64::NAN), 0);
        // Wheel up (negative delta) reads older rows: a positive row count.
        assert_eq!(rows_for_pixels(-100.0), 5);
        assert_eq!(rows_for_pixels(100.0), -5);
        // Less than one row still moves one row.
        assert_eq!(rows_for_pixels(-3.0), 1);
        assert_eq!(rows_for_pixels(3.0), -1);
    }
}

#[cfg(test)]
mod shell_input_tests {
    use super::*;
    use modron_mobile_core::view::entry_lines;

    #[test]
    fn bang_is_shared_and_double_bang_is_private() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            submit_action("!ls -la"),
            UiAction::Shell { command: "ls -la".into(), private: false }
        );
        assert_eq!(submit_action("!!ls"), UiAction::Shell { command: "ls".into(), private: true });
        assert_eq!(
            submit_action("!!   pwd  "),
            UiAction::Shell { command: "pwd".into(), private: true }
        );
    }

    #[test]
    fn a_bare_bang_sends_an_empty_command_for_the_core_usage_refusal() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(submit_action("!"), UiAction::Shell { command: String::new(), private: false });
        assert_eq!(submit_action("!!"), UiAction::Shell { command: String::new(), private: true });
    }

    #[test]
    fn plain_text_and_commands_are_not_shell() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(submit_action("hello !x"), UiAction::Send("hello !x".into()));
        assert_eq!(submit_action("/help"), slash::run("/help"));
    }

    #[test]
    fn shell_rows_put_the_command_first_and_dim_the_output() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let entry = TranscriptEntry::Shell {
            command: "ls".into(),
            lines: vec!["a.txt".into(), "b.txt".into()],
            is_error: false,
            private: false,
        };
        let history = vec![RenderedRow { lines: entry_lines(&entry), entry }];
        let lines = transcript_lines(&history);
        let styles: Vec<Style> = lines.iter().map(|l| l.style).collect();
        assert_eq!(styles, vec![Style::Bold, Style::Dim, Style::Dim]);
        assert_eq!(lines[0].text, "$ ls");
    }

    #[test]
    fn private_rows_keep_the_shared_mark() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let entry = TranscriptEntry::Shell {
            command: "env".into(),
            lines: vec![],
            is_error: false,
            private: true,
        };
        let lines = entry_lines(&entry);
        assert_eq!(lines[0], format!("$ env [{}]", modron_view::PRIVATE_SHELL_MARK));
    }
}

/// One image staged for the next submit (PARITY item 10): the chip's name and
/// size, and the wire attachment the core sends.
#[derive(Clone, Debug)]
pub struct Staged {
    pub name: String,
    pub bytes: u64,
    pub wire: Attachment,
}

/// The images staged for the next submit. The driver clears them only when
/// the submit is accepted, so a refused submit keeps its chips.
static STAGED: Mutex<Vec<Staged>> = Mutex::new(Vec::new());

/// The chip text: `[img name · size]`, the size through the shared formatter.
pub fn chip_label(name: &str, bytes: u64) -> String {
    format!("[img {name} \u{b7} {}]", modron_view::human_bytes(bytes))
}

/// Stages an image for the next submit.
pub fn stage(item: Staged) {
    if let Ok(mut staged) = STAGED.lock() {
        staged.push(item);
    }
}

/// Removes the last staged image; true when one was removed.
pub fn unstage_last() -> bool {
    STAGED.lock().map(|mut s| s.pop().is_some()).unwrap_or(false)
}

/// Clears every staged image (after an accepted submit).
pub fn clear_staged() {
    if let Ok(mut staged) = STAGED.lock() {
        staged.clear();
    }
}

/// How many images are staged.
pub fn staged_count() -> usize {
    STAGED.lock().map(|s| s.len()).unwrap_or(0)
}

/// The wire attachments the next submit carries.
pub fn staged_wire() -> Vec<Attachment> {
    STAGED.lock().map(|s| s.iter().map(|i| i.wire.clone()).collect()).unwrap_or_default()
}

/// The chip labels, in staging order.
pub fn staged_labels() -> Vec<String> {
    STAGED
        .lock()
        .map(|s| s.iter().map(|i| chip_label(&i.name, i.bytes)).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod attachment_tests {
    use super::*;

    #[test]
    fn ctrl_v_pastes_and_backspace_drops_a_chip_before_a_queued_message() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let chip = KeyFacts { attachments: 1, queued: 2, ..Default::default() };
        assert_eq!(
            route_key(Key::Backspace, false, false, chip, ""),
            Routed::Action(UiAction::Detach)
        );
        let queued = KeyFacts { queued: 2, ..Default::default() };
        assert_eq!(
            route_key(Key::Backspace, false, false, queued, ""),
            Routed::Action(UiAction::Dequeue(1))
        );
        assert_eq!(
            route_key(Key::Char('v'), false, true, KeyFacts::default(), ""),
            Routed::Action(UiAction::Paste)
        );
    }

    #[test]
    fn chips_show_name_and_the_shared_size() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(chip_label("pic.png", 1229), "[img pic.png \u{b7} 1.2 KB]");
    }
}

#[cfg(test)]
mod view_floor_tests {
    use super::*;
    use crate::clock::Instant;
    use std::time::Duration;

    #[test]
    fn a_note_lasts_two_seconds_and_an_error_never_expires() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let start = Instant::now();
        assert!(!note_expired(start, start + Duration::from_millis(1999)));
        assert!(note_expired(start, start + Duration::from_millis(2000)));

        let raw = ViewModelSnapshot {
            last_note: Some("task created".into()),
            last_error: Some("refused".into()),
            ..Default::default()
        };
        assert_eq!(shown_copy(&raw, 0, false).last_note.as_deref(), Some("task created"));
        let expired = shown_copy(&raw, 0, true);
        assert_eq!(expired.last_note, None);
        assert_eq!(expired.last_error.as_deref(), Some("refused"));
    }

    #[test]
    fn clearing_past_the_end_of_the_history_hides_nothing_more() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let raw = ViewModelSnapshot::default();
        assert!(shown_copy(&raw, 5, false).history.is_empty());
    }

    #[test]
    fn the_clipboard_text_is_offered_once() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        offer_text("pasted".to_string());
        assert_eq!(take_offered_text().as_deref(), Some("pasted"));
        assert_eq!(take_offered_text(), None);
    }
}

#[cfg(test)]
mod esc_tests {
    use super::*;

    fn idle() -> KeyFacts {
        KeyFacts { screen: Screen::Chat, ..Default::default() }
    }

    fn esc(facts: KeyFacts, draft: &str) -> Routed {
        route_key(Key::Escape, false, false, facts, draft)
    }

    #[test]
    fn esc_precedence_covers_every_state() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        // A pending approval is denied first, even with text in the composer.
        assert_eq!(
            esc(KeyFacts { approval_options: Some(3), ..idle() }, "hi"),
            Routed::Action(UiAction::Decide(2))
        );
        // A pending plan takes nothing.
        assert_eq!(esc(KeyFacts { plan_pending: true, ..idle() }, "hi"), Routed::Ignored);
        // Open suggestions close; the text stays.
        assert_eq!(
            esc(KeyFacts { slash_len: 2, ..idle() }, "/co"),
            Routed::Nav(NavStep::SlashHide)
        );
        // A running turn is interrupted; the text stays.
        assert_eq!(
            esc(KeyFacts { running: true, ..idle() }, "hi"),
            Routed::Action(UiAction::Interrupt)
        );
        // Idle with text: the text is cleared.
        assert_eq!(esc(idle(), "hi"), Routed::ClearDraft);
        // Idle, recalling a sent input: the recalled text clears too.
        assert_eq!(
            esc(KeyFacts { recalling: true, history_len: 3, ..idle() }, "old"),
            Routed::ClearDraft
        );
        // Idle and empty: nothing.
        assert_eq!(esc(idle(), ""), Routed::Ignored);
    }

    #[test]
    fn esc_never_detaches_the_staged_chips() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        // Chips alone: nothing happens.
        assert_eq!(esc(KeyFacts { attachments: 2, ..idle() }, ""), Routed::Ignored);
        // Chips with text: only the text goes (Detach is Backspace's, never Esc's).
        assert_eq!(esc(KeyFacts { attachments: 2, ..idle() }, "hi"), Routed::ClearDraft);
    }
}

#[cfg(test)]
mod tick_tests {
    use super::*;

    #[test]
    fn a_running_turn_asks_for_the_animation_period_and_an_idle_app_asks_for_nothing() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(tick_deadline(true), Some(ANIM_PERIOD));
        assert_eq!(tick_deadline(false), None);
    }
}

#[cfg(test)]
mod credential_remove_key_tests {
    use super::*;

    #[test]
    fn d_on_the_credentials_screen_asks_to_remove_the_selected_row() {
        let _serial = super::tests::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let facts = KeyFacts { screen: Screen::Auth, selected_index: 1, ..Default::default() };
        assert_eq!(
            route_key(Key::Char('d'), false, false, facts, ""),
            Routed::Nav(NavStep::AskAuthRemove(1))
        );
    }
}
