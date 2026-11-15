//! The chat screen: a thin render model over `ViewModelSnapshot`.
//!
//! Every string shown comes from the client core (`modron-mobile-core`),
//! already formatted: this module only chooses which field goes where and
//! splits multi-line transcript rows into one visual line each. Nothing is
//! re-formatted here (AGENTS.md shared invariants).
//!
//! Keys: the host routes keys through [`Host::filter`] before the NSH view
//! sees them. NSH on a terminal has no global key hook, so these are taken
//! there: Esc (Interrupt), y / a / n (approval decisions), y / r (plan
//! decisions, while no approval waits) and Alt+Enter (queue). Everything
//! else reaches the composer. Enter sends (steers while a run is active).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use modron_mobile_core::{RenderedRow, ViewModelSnapshot};
use modron_view::TranscriptEntry;
use nsh_core::element::input;
use nsh_core::prelude::*;
#[cfg(not(target_arch = "wasm32"))]
use nsh_platform_tui::{Input, Modifiers};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

/// What the user asked the client to do. The host or the view sends these;
/// the session driver (`crate::session`) turns them into core calls.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiAction {
    /// Composer submit: a submit when idle, a steer while a run is active
    /// (the core's `send_message`).
    Send(String),
    /// Alt+Enter: hold the message for the session's next run.
    Queue(String),
    /// Remove one queued message by position (0 = next to run).
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
}

/// Key hints for the approval options, in protocol order. Keys are TUI
/// presentation (AGENTS.md divergence table); the labels are the core's.
const DECISION_KEYS: [&str; 3] = ["y", "a", "n"];

/// Key hints for the plan options, in protocol order (approve, revise).
const PLAN_KEYS: [&str; 2] = ["y", "r"];

/// Visible slots for queue chips, steer echoes and plan lines. Fixed slot
/// counts keep NSH's cell layout static (see the transcript note below).
const QUEUE_SLOTS: usize = 4;
const STEER_SLOTS: usize = 3;
const PLAN_LINE_SLOTS: usize = 12;

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

/// The plain text styles for now; colour comes with the theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    Plain,
    Bold,
    Meta,
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
}

impl Model {
    pub fn from_snapshot(s: &ViewModelSnapshot) -> Self {
        Self {
            connection: s.connection_label.clone(),
            provider: s.provider.clone(),
            running: s.running_label.clone(),
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
        }
    }
}

/// The visual style of a transcript row kind.
pub fn style_of(entry: &TranscriptEntry) -> Style {
    match entry {
        TranscriptEntry::Task { .. } => Style::Bold,
        TranscriptEntry::ToolSummary { .. }
        | TranscriptEntry::Swarm { .. }
        | TranscriptEntry::Model { .. }
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

/// The host's end of the link: pushes snapshots, takes actions, and filters
/// the keys that the view must not see.
pub struct Host {
    snapshots: UnboundedSender<ViewModelSnapshot>,
    actions: UnboundedReceiver<UiAction>,
    loopback: UnboundedSender<UiAction>,
    /// Option count of the pending approval, when one is pending.
    pending: Option<usize>,
    /// Whether a proposed plan waits for a decision.
    plan_pending: bool,
}

/// The view's end of the link, handed to `App` through `CLIENT`.
pub struct Client {
    pub snapshots: UnboundedReceiver<ViewModelSnapshot>,
    pub actions: UnboundedSender<UiAction>,
}

/// Creates the two ends of the link. Also resets the composer mirror, so a
/// new view starts from an empty draft.
pub fn link() -> (Host, Client) {
    set_draft("");
    QUEUE_NEXT.store(false, Ordering::SeqCst);
    CLEAR_DRAFT.store(false, Ordering::SeqCst);
    let (snap_tx, snap_rx) = unbounded_channel();
    let (act_tx, act_rx) = unbounded_channel();
    (
        Host {
            snapshots: snap_tx,
            actions: act_rx,
            loopback: act_tx.clone(),
            pending: None,
            plan_pending: false,
        },
        Client { snapshots: snap_rx, actions: act_tx },
    )
}

impl Host {
    /// Sends a snapshot to the view and records what the key filter needs:
    /// whether an approval or a plan waits.
    pub fn push(&mut self, snapshot: &ViewModelSnapshot) {
        self.pending = snapshot.pending_approval.as_ref().map(|p| p.options.len());
        self.plan_pending = snapshot.plan_pending.is_some();
        let _ = self.snapshots.send(snapshot.clone());
    }

    /// The next action the view produced, if any.
    pub fn try_action(&mut self) -> Option<UiAction> {
        self.actions.try_recv().ok()
    }

    /// Routes one terminal key. Returns `Some(input)` for the view to handle,
    /// or `None` when the key became an action here.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn filter(&self, input: Input) -> Option<Input> {
        let Input::Key { key, modifiers, release: false } = &input else {
            return Some(input);
        };
        if self.take_key(key, modifiers.alt, has_chord(modifiers)) {
            None
        } else {
            Some(input)
        }
    }

    /// Routes a key by name, the same rules on every host: Enter always
    /// reaches the composer (`alt` marks a queue), chords pass, Esc
    /// interrupts, `y`/`a`/`n` decide a pending approval, and `y`/`r` decide
    /// a pending plan. Returns true when the key became an action here.
    pub fn take_key(&self, key: &str, alt: bool, chord: bool) -> bool {
        if key == "Enter" {
            // The flag tells the composer's submit whether this was Alt+Enter.
            QUEUE_NEXT.store(alt, Ordering::SeqCst);
            return false;
        }
        if chord {
            return false;
        }
        if key == "Escape" {
            self.act(UiAction::Interrupt);
            return true;
        }
        if let Some(count) = self.pending {
            return match DECISION_KEYS.iter().position(|k| *k == key) {
                Some(index) if index < count => {
                    self.act(UiAction::Decide(index));
                    true
                }
                _ => false,
            };
        }
        if self.plan_pending {
            if let Some(action) = plan_action(key) {
                self.act(action);
                return true;
            }
        }
        false
    }

    fn act(&self, action: UiAction) {
        let _ = self.loopback.send(action);
    }
}

/// The plan decision for a key: `y` approves; `r` revises with the composer
/// text as the note (no note when the composer is empty).
fn plan_action(key: &str) -> Option<UiAction> {
    match key {
        k if k == PLAN_KEYS[0] => Some(UiAction::DecidePlan { approve: true, note: None }),
        k if k == PLAN_KEYS[1] => {
            let note = DRAFT.lock().map(|d| d.trim().to_string()).unwrap_or_default();
            let note = (!note.is_empty()).then_some(note);
            if note.is_some() {
                CLEAR_DRAFT.store(true, Ordering::SeqCst);
            }
            Some(UiAction::DecidePlan { approve: false, note })
        }
        _ => None,
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

fn is_style(model: Signal<Model>, at: Position, want: Style) -> bool {
    model.with(|m| m.lines.get(at.index()).is_some_and(|l| l.style == want))
}

fn line_text(model: Signal<Model>, at: Position) -> String {
    model.with(|m| m.lines.get(at.index()).map(|l| l.text.clone()).unwrap_or_default())
}

/// One transcript cell. Cells are recycled, so each region reads the row
/// position reactively and shows only while its style matches.
fn transcript_row(model: Signal<Model>, at: Position) -> impl IntoElement {
    view()
        .child(when(
            move || is_style(model, at, Style::Bold),
            move || text(move || line_text(model, at)).font_weight(FontWeight::BOLD),
        ))
        .child(when(
            move || is_style(model, at, Style::Meta),
            move || text(move || line_text(model, at)).italic(),
        ))
        .child(when(
            move || is_style(model, at, Style::Plain),
            move || text(move || line_text(model, at)),
        ))
}

fn header(model: Signal<Model>) -> impl IntoElement {
    view()
        .flex_row()
        .child(text(move || model.with(|m| m.connection.clone())))
        .child(text(move || model.with(|m| m.provider.clone())))
        .child(text(move || model.with(|m| m.running.clone())))
}

/// Up to `max` rows of `rows(model)`, one text element per slot. A slot
/// shows only while its row exists, so the layout stays fixed.
fn row_slots(
    model: Signal<Model>,
    max: usize,
    rows: fn(&Model) -> Vec<String>,
    italic: bool,
) -> impl IntoElement {
    let mut column = view().flex_col();
    for i in 0..max {
        column = column.child(when(
            move || model.with(|m| rows(m).len() > i),
            move || {
                let line =
                    text(move || model.with(|m| rows(m).get(i).cloned().unwrap_or_default()));
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

fn queue_rows(m: &Model) -> Vec<String> {
    m.queued.iter().enumerate().map(|(i, q)| format!("[{}] {q}", i + 1)).collect()
}

fn steer_rows(m: &Model) -> Vec<String> {
    m.pending_steers.iter().map(|s| format!("> {s}")).collect()
}

fn plan_line_rows(m: &Model) -> Vec<String> {
    m.plan.as_ref().map(|p| p.lines.clone()).unwrap_or_default()
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

fn plan_block(model: Signal<Model>) -> impl IntoElement {
    view().flex_col().child(row_slots(model, PLAN_LINE_SLOTS, plan_line_rows, false)).child(text(
        move || {
            model.with(|m| m.plan.as_ref().map(|p| plan_option_row(&p.labels)).unwrap_or_default())
        },
    ))
}

fn approval_strip(model: Signal<Model>) -> impl IntoElement {
    let option = move |i: usize| {
        text(move || {
            model.with(|m| {
                m.approval
                    .as_ref()
                    .and_then(|a| a.labels.get(i))
                    .map(|label| format!("{} {label}", DECISION_KEYS[i]))
                    .unwrap_or_default()
            })
        })
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

/// The chat screen. `actions` receives the composer's own actions (Send,
/// Queue, Compact); decisions, interrupts and plan answers come from the
/// host's key filter.
pub fn screen(
    model: Signal<Model>,
    draft: Signal<String>,
    actions: UnboundedSender<UiAction>,
) -> impl IntoElement {
    let composer = input()
        .auto_focus(true)
        // Enter sends and keeps focus, so the next line can be typed at once.
        .submit_behavior(SubmitBehavior::Submit)
        .placeholder("Message")
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
                let _ = actions.send(action);
            }
        });
    view()
        .flex_col()
        .size_full()
        .child(header(model))
        .child(text(move || model.with(|m| m.status_line.clone())))
        .child(
            virtual_list(
                move || model.with(|m| m.lines.len()),
                move |at: Position| transcript_row(model, at),
            )
            // An explicit height: a flex-grown virtual list gets zero rows
            // in NSH 0.5.1 (spikes/nsh-tui/RESULTS.md, section 4).
            .h(px(600.0)),
        )
        .child(row_slots(model, STEER_SLOTS, steer_rows, true))
        .child(when(
            move || model.with(|m| !m.streaming.is_empty()),
            move || text(move || model.with(|m| m.streaming.clone())),
        ))
        .child(when(move || model.with(|m| m.approval.is_some()), move || approval_strip(model)))
        .child(when(move || model.with(|m| m.plan.is_some()), move || plan_block(model)))
        .child(row_slots(model, QUEUE_SLOTS, queue_rows, false))
        .child(composer)
}

/// An empty send is dropped; every other action goes to the driver.
fn action_is_empty(action: &UiAction) -> bool {
    matches!(action, UiAction::Send(t) | UiAction::Queue(t) if t.is_empty())
}
