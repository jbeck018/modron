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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use modron_mobile_core::{
    ApprovalModeWire, Effort, Phase, RenderedRow, SandboxPolicy, ViewModelSnapshot,
};
use modron_view::{TranscriptEntry, COMPOSER_PLACEHOLDER, QUEUE_LABEL, STOP_LABEL};
use nsh_core::element::{input, AdaptiveColor};
use nsh_core::prelude::*;
#[cfg(not(target_arch = "wasm32"))]
use nsh_platform_tui::{Input, Modifiers};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use uuid::Uuid;

use crate::nav::{Nav, NavStep, NavView, Screen};

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
}

/// Key hints for the approval options, in protocol order. Keys are TUI
/// presentation (AGENTS.md divergence table); the labels are the core's.
const DECISION_KEYS: [&str; 3] = ["y", "a", "n"];

/// Key hints for the plan options, in protocol order (approve, revise).
const PLAN_KEYS: [&str; 2] = ["y", "r"];

/// True on the web host: tappable controls are drawn there only. The
/// terminal keeps its key hints as text (AGENTS.md divergence table).
pub(crate) const WEB_CONTROLS: bool = cfg!(target_arch = "wasm32");

/// Visible slots for queue chips, steer echoes, plan lines and the live
/// activity rows. Fixed slot counts keep NSH's cell layout static (see the
/// transcript note below).
const QUEUE_SLOTS: usize = 4;
const STEER_SLOTS: usize = 3;
const PLAN_LINE_SLOTS: usize = 12;
const TOOL_SLOTS: usize = 6;
const SWARM_SLOTS: usize = 4;
const NOTE_SLOTS: usize = 2;

/// The composer text, mirrored from the view so the host can read it for a
/// revise note (the host filters keys before the composer sees them).
static DRAFT: Mutex<String> = Mutex::new(String::new());
/// Set by the host on every Enter: the composer's next submit is a queue
/// when this is true (Alt+Enter).
static QUEUE_NEXT: AtomicBool = AtomicBool::new(false);
/// Set when a revise note was sent: the composer clears once the plan has
/// been resolved (see [`after_snapshot`]), so a refused revise keeps its note.
static CLEAR_DRAFT: AtomicBool = AtomicBool::new(false);

fn set_draft(text: &str) {
    if let Ok(mut draft) = DRAFT.lock() {
        draft.clear();
        draft.push_str(text);
    }
}

fn draft_text() -> String {
    DRAFT.lock().map(|d| d.clone()).unwrap_or_default()
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
    /// Any other key (dead keys, Tab): always the composer's.
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
        _ => key_of(name),
    }
}

/// The snapshot facts key routing needs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeyFacts {
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
}

/// Where a key goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Routed {
    /// The composer gets the key. `queue` is Alt on an Enter.
    Composer { queue: bool },
    /// The key became an action.
    Action(UiAction),
    /// The key is a screen step (Sessions, Settings, and the Ctrl+O / F2
    /// openers).
    Nav(NavStep),
    /// A key the open screen has no use for: consumed, nothing happens.
    Ignored,
}

/// The one key-to-action rule, shared by every host.
///
/// Order: F2 opens Settings and Ctrl+O opens Sessions (from any screen);
/// on Sessions and Settings every other key is the screen's
/// ([`route_screen_key`]); on Chat, Enter always reaches the composer (Alt
/// marks a queue); Backspace on an empty composer with messages queued
/// removes the last one; a chord passes; Esc interrupts; with an approval
/// pending, y / a / n decide its options and every other key is the
/// composer's; with a plan pending, y approves and r revises with the
/// trimmed composer text as the note.
///
/// Esc precedence: on Sessions, Esc clears a non-empty filter first and
/// closes the screen second; on Settings it closes the screen; on Chat it
/// interrupts. The composer's text is passed in, so the rule is testable
/// without a view.
pub fn route_key(key: Key, alt: bool, chord: bool, facts: KeyFacts, draft: &str) -> Routed {
    let composer = Routed::Composer { queue: false };
    match key {
        Key::F2 => return Routed::Nav(NavStep::Open(Screen::Settings)),
        Key::Char('o') if chord => return Routed::Nav(NavStep::Open(Screen::Sessions)),
        _ => {}
    }
    if facts.screen != Screen::Chat {
        return route_screen_key(key, chord, &facts);
    }
    match key {
        Key::Enter => return Routed::Composer { queue: alt },
        Key::Backspace if !chord && draft.is_empty() && facts.queued > 0 => {
            return Routed::Action(UiAction::Dequeue(facts.queued - 1));
        }
        Key::Other | Key::Backspace | Key::Up | Key::Down | Key::F2 => return composer,
        _ if chord => return composer,
        Key::Escape => return Routed::Action(UiAction::Interrupt),
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

/// Keys on the Sessions and Settings screens. There is no composer there,
/// so these keys are the screen's: arrows move, Enter activates, Esc backs
/// out, and on Sessions the printable keys type the filter (`n` and j / k
/// act only while the filter is empty). A chord still passes.
fn route_screen_key(key: Key, chord: bool, facts: &KeyFacts) -> Routed {
    if chord {
        return Routed::Composer { queue: false };
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
            Some(action) => Routed::Action(action.clone()),
            None => Routed::Ignored,
        },
        Key::Up => Routed::Nav(NavStep::Move(-1)),
        Key::Down => Routed::Nav(NavStep::Move(1)),
        Key::Backspace => Routed::Nav(NavStep::FilterBack),
        Key::Char('n') if sessions && !facts.filter_active => Routed::Nav(NavStep::NewSession),
        Key::Char('j') if sessions && !facts.filter_active => Routed::Nav(NavStep::Move(1)),
        Key::Char('k') if sessions && !facts.filter_active => Routed::Nav(NavStep::Move(-1)),
        Key::Char(c) if sessions => Routed::Nav(NavStep::Filter(c)),
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
}

impl Model {
    pub fn from_snapshot(s: &ViewModelSnapshot) -> Self {
        Self {
            connection: s.connection_label.clone(),
            provider: s.provider.clone(),
            running: s.running_label.clone(),
            is_running: s.running,
            status_line: s.status_line.clone(),
            lines: transcript_lines(&s.history),
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
        }
    }
}

/// The visual style of a transcript row kind.
pub fn style_of(entry: &TranscriptEntry) -> Style {
    match entry {
        TranscriptEntry::Task { .. } => Style::Bold,
        TranscriptEntry::ToolSummary { .. } | TranscriptEntry::Swarm { .. } => Style::Tool,
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
            row.lines.iter().map(move |text| Line { style, text: text.clone() })
        })
        .collect()
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
    nav: Nav,
}

/// Creates the two ends of the link. Also resets the composer mirror, so a
/// new view starts from an empty draft.
pub fn link() -> (Host, Client) {
    set_draft("");
    QUEUE_NEXT.store(false, Ordering::SeqCst);
    CLEAR_DRAFT.store(false, Ordering::SeqCst);
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
            nav: Nav::default(),
        },
        Client { snapshots: snap_rx, actions: act_tx, nav: nav_rx },
    )
}

impl Host {
    /// Sends a snapshot to the view and records what the key routing needs:
    /// whether an approval or a plan waits, and the sessions and settings.
    pub fn push(&mut self, snapshot: &ViewModelSnapshot) {
        self.pending = snapshot.pending_approval.as_ref().map(|p| p.options.len());
        self.plan_pending = snapshot.plan_pending.is_some();
        self.queued = snapshot.queued.len();
        self.nav.observe(snapshot);
        let _ = self.snapshots.send(snapshot.clone());
        self.notify();
    }

    /// The next action for the driver, if any. Navigation steps (from taps)
    /// are applied here and never returned.
    pub fn try_action(&mut self) -> Option<UiAction> {
        loop {
            match self.actions.try_recv().ok()? {
                UiAction::Nav(step) => self.nav_step(step),
                action => return Some(action),
            }
        }
    }

    /// Routes one terminal key. Returns `Some(input)` for the view to handle,
    /// or `None` when the key became an action or a screen step here.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn filter(&mut self, input: Input) -> Option<Input> {
        let Input::Key { key, modifiers, release: false } = &input else {
            return Some(input);
        };
        if self.route(host_key(key), modifiers.alt, has_chord(modifiers)) {
            None
        } else {
            Some(input)
        }
    }

    /// Routes a key by name: [`Host::route`] with [`host_key`].
    pub fn take_key(&mut self, key: &str, alt: bool, chord: bool) -> bool {
        self.route(host_key(key), alt, chord)
    }

    /// Routes one key, the same on every host. Returns true when the host
    /// consumes the key (an action, a screen step, or an ignored key); false
    /// when the composer gets it.
    pub fn route(&mut self, key: Key, alt: bool, chord: bool) -> bool {
        let facts = KeyFacts {
            approval_options: self.pending,
            plan_pending: self.plan_pending,
            queued: self.queued,
            screen: self.nav.top(),
            filter_active: !self.nav.filter().is_empty(),
            selected_session: self.nav.selected_session(),
            enter_setting: self.nav.enter_setting(),
        };
        match route_key(key, alt, chord, facts, &draft_text()) {
            Routed::Composer { queue } => {
                if key == Key::Enter {
                    // The flag tells the composer's submit whether this was Alt+Enter.
                    QUEUE_NEXT.store(queue, Ordering::SeqCst);
                }
                false
            }
            Routed::Action(action) => {
                arm_plan_clear(&action);
                self.act(action);
                true
            }
            Routed::Nav(step) => {
                self.nav_step(step);
                true
            }
            Routed::Ignored => true,
        }
    }

    /// Applies a screen step now (so the next key sees its result), sends
    /// the core calls it implies, and tells the view.
    fn nav_step(&mut self, step: NavStep) {
        for action in self.nav.apply(step) {
            self.act(action);
        }
        self.notify();
    }

    fn notify(&self) {
        let _ = self.navs.send(self.nav.view());
    }

    fn act(&self, action: UiAction) {
        let _ = self.loopback.send(action);
    }
}

/// Clears the composer after a revise note went out, once the plan is no
/// longer pending. The view calls this for every snapshot it receives.
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
        Style::Plain | Style::Tool => line,
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

fn queue_rows(m: &Model) -> Vec<String> {
    m.queued.iter().enumerate().map(|(i, q)| format!("[{}] {q}", i + 1)).collect()
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
    m.pending_steers.iter().map(|s| format!("> {s}")).collect()
}

fn plan_line_rows(m: &Model) -> Vec<String> {
    m.plan.as_ref().map(|p| p.lines.clone()).unwrap_or_default()
}

/// The last `TOOL_SLOTS` tool rows: the live strip shows the latest activity.
fn tool_rows(m: &Model) -> Vec<String> {
    let start = m.tool_rows.len().saturating_sub(TOOL_SLOTS);
    m.tool_rows[start..].to_vec()
}

fn swarm_rows(m: &Model) -> Vec<String> {
    m.swarm_rows.clone()
}

fn note_rows(m: &Model) -> Vec<String> {
    m.notes.clone()
}

/// The live tool strip: the turn summary line, then the latest tool rows.
/// Shown while the turn has tool activity; the committed `ToolSummary` row
/// takes over in the transcript once the turn ends.
fn tool_strip(model: Signal<Model>) -> impl IntoElement {
    view()
        .flex_col()
        .child(
            text(move || model.with(|m| m.turn_summary.clone().unwrap_or_default()))
                .text_color(move || token_ink(tool_token(""))),
        )
        .child(row_slots(model, TOOL_SLOTS, tool_rows, tool_row_token, false))
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
    actions: UnboundedSender<UiAction>,
) -> impl IntoElement {
    let submit_actions = actions.clone();
    let composer = input()
        .auto_focus(true)
        // Enter sends and keeps focus, so the next line can be typed at once.
        .submit_behavior(SubmitBehavior::Submit)
        .placeholder(COMPOSER_PLACEHOLDER)
        .value(move || draft.get())
        .on_change(move |s: &str| {
            set_draft(s);
            draft.set(s.to_string());
        })
        .on_submit(move |s: &str| {
            let task = s.trim().to_string();
            set_draft("");
            draft.set(String::new());
            // The host set this on the Enter that produced the submit.
            let queue = QUEUE_NEXT.swap(false, Ordering::SeqCst);
            let action = if task == "/compact" {
                UiAction::Compact
            } else if queue {
                UiAction::Queue(task)
            } else {
                UiAction::Send(task)
            };
            if !action_is_empty(&action) {
                let _ = submit_actions.send(action);
            }
        });
    view()
        .flex_col()
        .size_full()
        .child(
            virtual_list(
                move || model.with(|m| m.lines.len()),
                move |at: Position| transcript_row(model, at),
            )
            // An explicit height: a flex-grown virtual list gets zero rows
            // in NSH 0.5.1 (spikes/nsh-tui/RESULTS.md, section 4).
            .h(px(600.0)),
        )
        .child(when(move || model.with(|m| m.turn_summary.is_some()), move || tool_strip(model)))
        .child(row_slots(model, SWARM_SLOTS, swarm_rows, tool_token, false))
        .child(row_slots(model, NOTE_SLOTS, note_rows, dim_token, true))
        .child(row_slots(model, STEER_SLOTS, steer_rows, dim_token, true))
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
        .child(queue_chips(model, actions.clone()))
        .child(composer)
        .child(composer_controls(model, draft, actions))
}

/// An empty send is dropped; every other action goes to the driver.
fn action_is_empty(action: &UiAction) -> bool {
    matches!(action, UiAction::Send(t) | UiAction::Queue(t) if t.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nav::Screen;

    const NONE: KeyFacts = KeyFacts {
        approval_options: None,
        plan_pending: false,
        queued: 0,
        screen: Screen::Chat,
        filter_active: false,
        selected_session: None,
        enter_setting: None,
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
        Routed::Composer { queue: false }
    }

    #[test]
    fn key_names_map_to_keys() {
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
    fn letters_reach_the_composer_when_nothing_is_pending() {
        for c in ['y', 'a', 'n', 'r', 'x', ' '] {
            assert_eq!(route_key(Key::Char(c), false, false, NONE, ""), composer(), "{c}");
        }
        assert_eq!(route_key(Key::Other, false, false, plan(), ""), composer());
    }

    #[test]
    fn escape_interrupts_whatever_is_pending() {
        let want = Routed::Action(UiAction::Interrupt);
        assert_eq!(route_key(Key::Escape, false, false, NONE, ""), want);
        assert_eq!(route_key(Key::Escape, false, false, approval(3), ""), want);
        assert_eq!(route_key(Key::Escape, false, false, plan(), ""), want);
    }

    #[test]
    fn enter_reaches_the_composer_and_alt_marks_a_queue() {
        assert_eq!(
            route_key(Key::Enter, false, false, approval(3), "x"),
            Routed::Composer { queue: false }
        );
        assert_eq!(route_key(Key::Enter, true, true, NONE, ""), Routed::Composer { queue: true });
    }

    #[test]
    fn chords_pass_to_the_composer() {
        assert_eq!(route_key(Key::Char('a'), false, true, approval(3), ""), composer());
        assert_eq!(route_key(Key::Escape, false, true, NONE, ""), composer());
    }

    #[test]
    fn approval_keys_decide_their_options_in_order() {
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
        assert_eq!(route_key(Key::Char('n'), false, false, approval(2), ""), composer());
    }

    #[test]
    fn an_approval_takes_precedence_over_a_plan() {
        let both = KeyFacts { approval_options: Some(3), plan_pending: true, ..NONE };
        assert_eq!(
            route_key(Key::Char('y'), false, false, both.clone(), ""),
            Routed::Action(UiAction::Decide(0))
        );
        assert_eq!(route_key(Key::Char('r'), false, false, both.clone(), "note"), composer());
    }

    #[test]
    fn plan_keys_approve_and_revise_with_the_draft_as_note() {
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
        assert_eq!(
            route_key(Key::Char('r'), false, false, plan(), "   "),
            Routed::Action(UiAction::DecidePlan { approve: false, note: None })
        );
    }

    #[test]
    fn plan_pending_leaves_other_letters_to_the_composer() {
        assert_eq!(route_key(Key::Char('a'), false, false, plan(), ""), composer());
        assert_eq!(route_key(Key::Char('n'), false, false, plan(), ""), composer());
    }

    #[test]
    fn backspace_on_an_empty_composer_dequeues_the_last_message() {
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
        assert_eq!(route_key(Key::Backspace, false, false, NONE, ""), composer());
    }

    #[test]
    fn backspace_with_composer_text_edits_the_text() {
        assert_eq!(route_key(Key::Backspace, false, false, queued(2), "fix"), composer());
    }

    #[test]
    fn backspace_with_a_chord_is_the_composers() {
        assert_eq!(route_key(Key::Backspace, false, true, queued(2), ""), composer());
    }

    #[test]
    fn backspace_dequeues_while_an_approval_waits() {
        let both = KeyFacts { approval_options: Some(3), queued: 2, ..NONE };
        assert_eq!(
            route_key(Key::Backspace, false, false, both.clone(), ""),
            Routed::Action(UiAction::Dequeue(1))
        );
    }

    #[test]
    fn plan_answer_for_matches_the_key_rule() {
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
        assert_eq!(host_key("ArrowUp"), Key::Up);
        assert_eq!(host_key("ArrowDown"), Key::Down);
        assert_eq!(host_key("F2"), Key::F2);
        assert_eq!(host_key("Enter"), Key::Enter);
        assert_eq!(host_key("ArrowLeft"), Key::Other);
    }

    #[test]
    fn esc_clears_a_filter_before_it_closes_sessions() {
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
        assert_eq!(
            route_key(Key::Escape, false, false, settings(None), ""),
            Routed::Nav(NavStep::Back)
        );
        assert_eq!(
            route_key(Key::Escape, false, false, NONE, ""),
            Routed::Action(UiAction::Interrupt)
        );
    }

    #[test]
    fn sessions_typing_filters_and_n_or_jk_act_only_on_an_empty_filter() {
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
        assert_eq!(
            route_key(Key::Enter, false, false, sessions(false), ""),
            Routed::Nav(NavStep::Attach(uuid::Uuid::from_u128(7)))
        );
        let empty = KeyFacts { screen: Screen::Sessions, ..NONE };
        assert_eq!(route_key(Key::Enter, false, false, empty, ""), Routed::Ignored);
    }

    #[test]
    fn enter_on_settings_sends_the_row_action_or_nothing() {
        let effort = UiAction::SetEffort(Some(Effort::Low));
        assert_eq!(
            route_key(Key::Enter, false, false, settings(Some(effort.clone())), ""),
            Routed::Action(effort)
        );
        assert_eq!(route_key(Key::Enter, false, false, settings(None), ""), Routed::Ignored);
    }

    #[test]
    fn arrows_move_on_sessions_and_settings() {
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
        assert_eq!(route_key(Key::Char('a'), false, false, settings(None), ""), Routed::Ignored);
    }

    #[test]
    fn a_chord_passes_to_the_composer_on_every_screen() {
        assert_eq!(
            route_key(Key::Char('a'), false, true, sessions(false), ""),
            Routed::Composer { queue: false }
        );
        assert_eq!(
            route_key(Key::Escape, false, true, settings(None), ""),
            Routed::Composer { queue: false }
        );
    }

    #[test]
    fn chat_keys_are_unchanged_by_the_new_screens() {
        assert_eq!(route_key(Key::Up, false, false, NONE, ""), Routed::Composer { queue: false });
        assert_eq!(
            route_key(Key::Char('n'), false, false, NONE, ""),
            Routed::Composer { queue: false }
        );
    }
}
