//! The chat screen: a thin render model over `ViewModelSnapshot`.
//!
//! Every string shown comes from the client core (`modron-mobile-core`),
//! already formatted: this module only chooses which field goes where and
//! splits multi-line transcript rows into one visual line each. Nothing is
//! re-formatted here (AGENTS.md shared invariants).
//!
//! Keys: the host routes keys through [`Host::filter`] before the NSH view
//! sees them. NSH on a terminal has no global key hook, so Esc (Interrupt)
//! and y / a / n (approval decisions) are taken there; everything else
//! reaches the composer.

use modron_mobile_core::{RenderedRow, ViewModelSnapshot};
use modron_view::TranscriptEntry;
use nsh_core::element::input;
use nsh_core::prelude::*;
use nsh_platform_tui::{Input, Modifiers};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

/// What the user asked the client to do. The host or the view sends these;
/// the transport (not in this packet) carries them to the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiAction {
    Submit(String),
    /// Index into the pending approval's `options` (protocol order).
    Decide(usize),
    Interrupt,
}

/// Key hints for the approval options, in protocol order. Keys are TUI
/// presentation (AGENTS.md divergence table); the labels are the core's.
const DECISION_KEYS: [&str; 3] = ["y", "a", "n"];

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

/// Everything the screen renders, derived 1:1 from one snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Model {
    pub connection: String,
    pub provider: String,
    pub running: String,
    pub status_line: String,
    pub lines: Vec<Line>,
    pub streaming: String,
    pub approval: Option<Approval>,
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
}

/// The view's end of the link, handed to `App` through `CLIENT`.
pub struct Client {
    pub snapshots: UnboundedReceiver<ViewModelSnapshot>,
    pub actions: UnboundedSender<UiAction>,
}

/// Creates the two ends of the link.
pub fn link() -> (Host, Client) {
    let (snap_tx, snap_rx) = unbounded_channel();
    let (act_tx, act_rx) = unbounded_channel();
    (
        Host { snapshots: snap_tx, actions: act_rx, loopback: act_tx.clone(), pending: None },
        Client { snapshots: snap_rx, actions: act_tx },
    )
}

impl Host {
    /// Sends a snapshot to the view and records whether an approval waits.
    pub fn push(&mut self, snapshot: &ViewModelSnapshot) {
        self.pending = snapshot.pending_approval.as_ref().map(|p| p.options.len());
        let _ = self.snapshots.send(snapshot.clone());
    }

    /// The next action the view produced, if any.
    pub fn try_action(&mut self) -> Option<UiAction> {
        self.actions.try_recv().ok()
    }

    /// Routes one key. Returns `Some(input)` for the view to handle, or
    /// `None` when the key became an action here (Esc, or y / a / n while an
    /// approval is pending).
    pub fn filter(&self, input: Input) -> Option<Input> {
        let Input::Key { key, modifiers, release: false } = &input else {
            return Some(input);
        };
        if has_chord(modifiers) {
            return Some(input);
        }
        if key == "Escape" {
            self.act(UiAction::Interrupt);
            return None;
        }
        let Some(count) = self.pending else {
            return Some(input);
        };
        let Some(index) = DECISION_KEYS.iter().position(|k| *k == key.as_str()) else {
            return Some(input);
        };
        if index >= count {
            return Some(input);
        }
        self.act(UiAction::Decide(index));
        None
    }

    fn act(&self, action: UiAction) {
        let _ = self.loopback.send(action);
    }
}

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

/// The chat screen. `actions` receives Submit from the composer; decisions
/// and interrupts come from the host's key filter.
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
        .on_change(move |s: &str| draft.set(s.to_string()))
        .on_submit(move |s: &str| {
            let task = s.trim().to_string();
            draft.set(String::new());
            if !task.is_empty() {
                let _ = actions.send(UiAction::Submit(task));
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
        .child(when(
            move || model.with(|m| !m.streaming.is_empty()),
            move || text(move || model.with(|m| m.streaming.clone())),
        ))
        .child(when(move || model.with(|m| m.approval.is_some()), move || approval_strip(model)))
        .child(composer)
}
