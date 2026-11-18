//! The web target's chat (Program UX-2): the top bar with its details row,
//! the transcript cells, and the composer dock (status strip, popovers,
//! decisions, the composer). Built from the `ui.rs` components on `nsh_base`,
//! in the tokens of `app/DESIGN.md`.
//!
//! Presentation only. The words, roles, placeholders, ordering and semantics
//! come from `chat.rs` and the shared crates (AGENTS.md "UI/UX parity
//! contract"); the decisions go through the same guard and actions as the
//! keys. Nothing here runs on the terminal: `chat::screen` and `frame` call
//! these functions only when `WEB_CONTROLS` is true.

use std::rc::Rc;

use modron_mobile_core::view::{
    connection_label, queue_chip, welcome_model_line, welcome_title, ConnectionState,
    CHAT_TITLE_LABEL, HELP_TITLE, RECONNECTING_LABEL, REMOVE_LABEL, RETRY_NOW_LABEL, SEND_LABEL,
    SESSIONS_LABEL, SESSION_DETAIL_LABEL, SETTINGS_LABEL, WELCOME_TAGLINE,
};
use modron_view::{QUEUE_LABEL, STOP_LABEL};
use nsh_core::prelude::*;
use tokio::sync::mpsc::UnboundedSender;

use crate::chat::{self, Model, Style, UiAction};
use crate::clock::Instant;
use crate::nav::{NavStep, NavView, Screen};
use crate::suggest;
use crate::ui::{self, Size, Tone, Variant};

/// Rows of the command or file suggestions popover (the rest scroll).
pub const POPOVER_ROWS: usize = 5;
/// Rows of the queued-messages popover.
const QUEUE_ROWS: usize = 4;
/// Chips of staged attachments above the composer.
const ATTACH_ROWS: usize = 4;
/// Notes shown above the composer (the core's last note).
const NOTE_ROWS: usize = 2;
/// Blocks (prose or code) one transcript row may hold before it shows raw.
const BLOCK_SLOTS: usize = 8;
/// The monospace stack: the theme names no font family.
const MONO: &str = "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace";
/// Space between transcript rows, and the composer's line height.
const ROW_GAP: f32 = 12.0;
const COMPOSER_LINE_PX: f32 = 24.0;

/// The bar's height, the header row and the status strip, in points (the
/// transcript's height is the window less these and the dock).
const BAR_H: f32 = ui::HEADER_H;
const STRIP_H: f32 = 40.0;
const DOCK_BASE_H: f32 = 8.0 + 8.0 + 44.0 + 8.0 + 44.0 + 8.0 + 8.0;
const DETAILS_H: f32 = 40.0;
/// The banner's height per connection word: a warning line with a skeleton
/// (connecting), the same with a retry button (reconnecting), a destructive
/// line (disconnected). The dock's estimate uses these.
const BANNER_CONNECTING_H: f32 = 72.0;
const BANNER_RETRY_H: f32 = 124.0;
const BANNER_DISCONNECTED_H: f32 = 56.0;
const TOOL_PANEL_H: f32 = 240.0;
const POPOVER_ROW_H: f32 = 60.0;
const APPROVAL_H: f32 = 180.0;
const PLAN_BASE_H: f32 = 120.0;
const MIN_TRANSCRIPT_H: f32 = 120.0;

/// One piece of an assistant row: prose, or a fenced code block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    Text(String),
    Code(String),
}

/// Splits assistant text at its fenced code blocks (```` ``` ````). A fence
/// with no close runs to the end (a stream in flight). The fence lines
/// themselves are not shown: they are markup, the code is the content.
pub fn blocks(raw: &str) -> Vec<Block> {
    let mut out = Vec::new();
    let mut prose: Vec<&str> = Vec::new();
    let mut code: Option<Vec<&str>> = None;
    for line in raw.split('\n') {
        let fence = line.trim_start().starts_with("```");
        if let Some(body) = code.as_mut() {
            if fence {
                let done = std::mem::take(body);
                out.push(Block::Code(done.join("\n")));
                code = None;
            } else {
                body.push(line);
            }
        } else if fence {
            push_prose(&mut out, &mut prose);
            code = Some(Vec::new());
        } else {
            prose.push(line);
        }
    }
    match code {
        Some(body) => out.push(Block::Code(body.join("\n"))),
        None => push_prose(&mut out, &mut prose),
    }
    out
}

fn push_prose(out: &mut Vec<Block>, prose: &mut Vec<&str>) {
    let text = prose.join("\n");
    if !text.trim().is_empty() {
        out.push(Block::Text(text.trim_matches('\n').to_string()));
    }
    prose.clear();
}

/// The blocks a row shows: `blocks`, or the raw text when it has more than
/// the slots hold (nothing is dropped).
fn slots(raw: &str) -> Vec<Block> {
    let parts = blocks(raw);
    if parts.len() > BLOCK_SLOTS {
        vec![Block::Text(raw.to_string())]
    } else {
        parts
    }
}

/// The first `·`-separated fact of a status line: `connected` of
/// `connected · ws://…`.
fn first_fact(line: &str) -> &str {
    line.split(" \u{b7} ").next().unwrap_or("")
}

/// The facts that are present, joined with the middle dot.
fn join_facts(facts: &[&str]) -> String {
    facts.iter().filter(|f| !f.is_empty()).copied().collect::<Vec<_>>().join(" \u{b7} ")
}

fn line_text(model: Signal<Model>, at: nsh_core::element::Position) -> String {
    model.with(|m| m.lines.get(at.index()).map(|l| l.text.clone()).unwrap_or_default())
}

fn is_style(model: Signal<Model>, at: nsh_core::element::Position, want: Style) -> bool {
    model.with(|m| m.lines.get(at.index()).is_some_and(|l| l.style == want))
}

/// Whether the line `delta` rows away is not a task row (a card's edge).
fn task_edge(model: Signal<Model>, at: nsh_core::element::Position, delta: isize) -> bool {
    model.with(|m| {
        let i = at.index() as isize + delta;
        i < 0 || m.lines.get(i as usize).is_none_or(|l| l.style != Style::Bold)
    })
}

/// The top bar (web): sessions, the title and the status, settings and help,
/// then the banner while the connection is not up. Tapping the status opens
/// the details row (the connection address and the session id).
pub fn bar(
    model: Signal<Model>,
    actions: UnboundedSender<UiAction>,
    details: Signal<bool>,
) -> impl IntoElement {
    let state = move || model.with(|m| first_fact(&m.connection).to_string());
    let tone = move || connection_tone(&state());
    // The title block is the status's press target (the details row): at
    // least 44 pt tall, the name and the status line inside it.
    let title = view()
        .role(AccessibilityRole::Button)
        .flex_col()
        .flex_1()
        .min_w(px(0.0))
        .min_h(px(ui::TOUCH_H))
        .justify_center()
        .px(px(4.0))
        .cursor_pointer()
        .on_press(move || details.set(!details.get()))
        .child(
            text(CHAT_TITLE_LABEL)
                .text_size(px(15.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(ui::tok("foreground"))
                .truncate(),
        )
        .child(
            view()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .min_w(px(0.0))
                .child(ui::status_dot(tone))
                .child(ui::caption(state).truncate()),
        );
    let row = ui::gutter(view().flex_row().items_center().gap(px(4.0)).w_full())
        .min_h(px(BAR_H))
        .max_w(px(ui::LIST_WIDTH))
        .self_center()
        .child(bar_action(SESSIONS_LABEL, actions.clone(), Screen::Sessions))
        .child(title)
        .child(bar_action(SETTINGS_LABEL, actions.clone(), Screen::Settings))
        .child(bar_action(HELP_TITLE, actions.clone(), Screen::Help));
    let details_row = when(
        move || details.get(),
        move || {
            ui::gutter(view().flex_col().gap(px(2.0)).pb(px(8.0)).w_full())
                .max_w(px(ui::LIST_WIDTH))
                .self_center()
                .child(ui::caption(move || model.with(|m| m.connection.clone())).truncate())
                .child(
                    ui::caption(move || {
                        model.with(|m| format!("{SESSION_DETAIL_LABEL} {}", m.session))
                    })
                    .truncate(),
                )
        },
    );
    view()
        .flex_col()
        .w_full()
        .bg(ui::tok("background"))
        .child(
            view()
                .flex_col()
                .w_full()
                .border_b_1()
                .border_color(ui::tok("border"))
                .child(row)
                .child(details_row),
        )
        .child(banner(model, actions))
}

/// The tone of a connection word (the bar's dot, the banner's alert): the
/// connected word is success; the words of a link that is coming up
/// (connecting, and reconnecting after a drop) are warning; any other word is
/// destructive.
pub fn connection_tone(word: &str) -> Tone {
    if word == connection_label(&ConnectionState::Connected) {
        Tone::Success
    } else if word == connection_label(&ConnectionState::Connecting) || word == RECONNECTING_LABEL {
        Tone::Warning
    } else {
        Tone::Destructive
    }
}

/// The banner's height for a connection word (0 when connected).
pub fn banner_px(word: &str) -> f32 {
    if word == RECONNECTING_LABEL {
        BANNER_RETRY_H
    } else if word == connection_label(&ConnectionState::Connecting) {
        BANNER_CONNECTING_H
    } else if word == connection_label(&ConnectionState::Connected) {
        0.0
    } else {
        BANNER_DISCONNECTED_H
    }
}

/// A bar control: a compact ghost button that opens `screen`.
fn bar_action(
    label: &'static str,
    actions: UnboundedSender<UiAction>,
    screen: Screen,
) -> impl IntoElement {
    ui::button(label, Variant::Ghost, Size::Sm, move || {
        let _ = actions.send(UiAction::Nav(NavStep::Open(screen)));
    })
}

/// The connection banner: `connecting` (warning, with a skeleton line),
/// `reconnecting` (the same, with `retry now`) or `disconnected` (destructive).
/// Nothing while connected.
fn banner(model: Signal<Model>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    let state = move || model.with(|m| first_fact(&m.connection).to_string());
    let connecting = move || state() == connection_label(&ConnectionState::Connecting);
    let reconnecting = move || state() == RECONNECTING_LABEL;
    let disconnected = move || state() == connection_label(&ConnectionState::Disconnected);
    view()
        .flex_col()
        .child(when(
            move || connecting() || reconnecting(),
            move || {
                let retry = actions.clone();
                ui::gutter(view().flex_col().gap(px(8.0)).py(px(8.0)).w_full())
                    .max_w(px(ui::LIST_WIDTH))
                    .self_center()
                    .child(ui::alert(Tone::Warning, state))
                    .child(ui::skeleton(8.0))
                    .child(when(reconnecting, move || {
                        let retry = retry.clone();
                        ui::button(RETRY_NOW_LABEL, Variant::Outline, Size::Sm, move || {
                            let _ = retry.send(UiAction::RetryConnection);
                        })
                        .w_full()
                    }))
            },
        ))
        .child(when(disconnected, move || {
            ui::gutter(view().flex_col().py(px(8.0)).w_full())
                .max_w(px(ui::LIST_WIDTH))
                .self_center()
                .child(ui::alert(Tone::Destructive, state))
        }))
}

/// One transcript row (web). Each style has its own presentation: a task is a
/// card, assistant text is body prose with its code blocks in a bordered mono
/// block that scrolls sideways, a tool summary is a compact bordered row,
/// model and usage notes are centred captions, shell output is mono, and the
/// in-flight text carries an accent rule.
pub fn cell(model: Signal<Model>, at: nsh_core::element::Position) -> impl IntoElement {
    ui::gutter(view().flex_col().w_full().max_w(px(ui::LIST_WIDTH)).self_center())
        .style_when(move || at.index() == 0, |s| s.pt(px(ROW_GAP + 4.0)))
        .child(when(move || is_style(model, at, Style::Bold), move || task(model, at)))
        .child(when(move || is_style(model, at, Style::Plain), move || prose_row(model, at)))
        .child(when(move || is_style(model, at, Style::Meta), move || meta(model, at)))
        .child(when(move || is_style(model, at, Style::Tool), move || tool(model, at)))
        .child(when(move || is_style(model, at, Style::Dim), move || dim(model, at)))
        .child(when(move || is_style(model, at, Style::Welcome), move || welcome(model, at)))
}

/// The web's welcome for a blank session: the app's title and version, the
/// tagline, and the model line (the row's text is the model's name). The
/// words are the shared welcome words.
fn welcome(model: Signal<Model>, at: nsh_core::element::Position) -> impl IntoElement {
    let model_name =
        move || model.with(|m| m.lines.get(at.index()).map(|l| l.text.clone()).unwrap_or_default());
    ui::welcome_state(welcome_title(env!("CARGO_PKG_VERSION")), WELCOME_TAGLINE, move || {
        welcome_model_line(&model_name())
    })
}

/// A task: a card row. The first and last rows of a run of task rows carry the
/// card's top and bottom padding, so a multi-line task reads as one card.
fn task(model: Signal<Model>, at: nsh_core::element::Position) -> impl IntoElement {
    view()
        .px(px(12.0))
        .bg(ui::tok("secondary"))
        .style_when(move || task_edge(model, at, -1), |s| s.pt(px(12.0)))
        .style_when(move || task_edge(model, at, 1), |s| s.pb(px(12.0)))
        .child(
            text(move || line_text(model, at))
                .text_size(px(16.0))
                .line_height(px(23.2))
                .text_color(ui::tok("foreground")),
        )
}

fn block_at(model: Signal<Model>, at: nsh_core::element::Position, slot: usize) -> Option<Block> {
    model.with(|m| m.lines.get(at.index()).and_then(|l| slots(&l.text).into_iter().nth(slot)))
}

/// Assistant prose: its blocks in order, paragraphs spaced by the row gap.
fn prose_row(model: Signal<Model>, at: nsh_core::element::Position) -> impl IntoElement {
    let mut column = view().flex_col().pb(px(ROW_GAP));
    for slot in 0..BLOCK_SLOTS {
        column = column.child(when(
            move || block_at(model, at, slot).is_some(),
            move || block_slot(model, at, slot),
        ));
    }
    column
}

fn block_slot(
    model: Signal<Model>,
    at: nsh_core::element::Position,
    slot: usize,
) -> impl IntoElement {
    view()
        .flex_col()
        .child(when(
            move || matches!(block_at(model, at, slot), Some(Block::Code(_))),
            move || code_block(model, at, slot),
        ))
        .child(when(
            move || matches!(block_at(model, at, slot), Some(Block::Text(_))),
            move || prose_text(model, at, slot),
        ))
}

fn prose_text(
    model: Signal<Model>,
    at: nsh_core::element::Position,
    slot: usize,
) -> impl IntoElement {
    view().pb(px(8.0)).child(
        text(move || match block_at(model, at, slot) {
            Some(Block::Text(body)) => body,
            _ => String::new(),
        })
        .text_size(px(16.0))
        .line_height(px(24.0))
        .text_color(ui::tok("foreground")),
    )
}

/// A code block: a bordered muted panel, the code in mono, scrolling sideways.
fn code_block(
    model: Signal<Model>,
    at: nsh_core::element::Position,
    slot: usize,
) -> impl IntoElement {
    view()
        .flex_col()
        .mb(px(8.0))
        .rounded(px(ui::radius()))
        .border_1()
        .border_color(ui::tok("border"))
        .bg(ui::tok("muted"))
        .overflow_hidden()
        .child(
            scroll().horizontal().child(
                view().p(px(12.0)).child(
                    text(move || match block_at(model, at, slot) {
                        Some(Block::Code(body)) => body,
                        _ => String::new(),
                    })
                    .font_family(MONO)
                    .text_size(px(13.0))
                    .line_height(px(18.8))
                    .text_color(ui::tok("foreground")),
                ),
            ),
        )
}

/// A model, usage or handoff note: a centred muted caption.
fn meta(model: Signal<Model>, at: nsh_core::element::Position) -> impl IntoElement {
    view().pb(px(ROW_GAP)).child(
        text(move || line_text(model, at))
            .text_size(px(13.0))
            .line_height(px(18.2))
            .text_color(ui::tok("muted-foreground"))
            .text_center(),
    )
}

/// A tool summary or sub-agent row: a compact bordered row, its colour from
/// the shared status mark (`chat::tool_row_token`).
fn tool(model: Signal<Model>, at: nsh_core::element::Position) -> impl IntoElement {
    view()
        .px(px(12.0))
        .py(px(8.0))
        .mb(px(4.0))
        .rounded(px(ui::radius()))
        .border_1()
        .border_color(ui::tok("border"))
        .bg(ui::tok("card"))
        .child(
            text(move || line_text(model, at))
                .text_size(px(13.0))
                .line_height(px(18.2))
                .text_color(move || chat::token_ink(chat::tool_row_token(&line_text(model, at)))),
        )
}

/// The turn's tool summary (web): a compact bordered row that toggles the
/// tool detail panel when pressed. The whole row is the target.
pub fn summary_row(line: impl IntoReactive<SharedString>, press: impl Fn() + 'static) -> View {
    view()
        .role(AccessibilityRole::Button)
        .min_h(px(ui::TOUCH_H))
        .px(px(12.0))
        .py(px(8.0))
        .mb(px(4.0))
        .rounded(px(ui::radius()))
        .border_1()
        .border_color(ui::tok("border"))
        .bg(ui::tok("card"))
        .cursor_pointer()
        .hover(|s| s.bg(ui::tok("accent")))
        .active(|s| s.opacity(0.75))
        .focus_visible(|s| s.ring(px(2.0)).ring_color(ui::tok("ring")))
        .on_press(press)
        .child(
            text(line)
                .text_size(px(13.0))
                .line_height(px(18.2))
                .text_color(move || chat::token_ink(chat::style_token(Style::Tool))),
        )
}

/// A tool detail or shell output row: mono, indented under its header.
fn dim(model: Signal<Model>, at: nsh_core::element::Position) -> impl IntoElement {
    view().pl(px(16.0)).pb(px(2.0)).child(
        text(move || line_text(model, at))
            .font_family(MONO)
            .text_size(px(13.0))
            .line_height(px(18.2))
            .text_color(ui::tok("muted-foreground")),
    )
}

/// The in-flight text below the transcript (web): the assistant's streaming
/// text in the prose look with an accent rule, or the opted-in reasoning in
/// muted italic under a neutral rule. It clears when the turn commits.
pub fn live_text(model: Signal<Model>, reasoning: bool) -> impl IntoElement {
    let body =
        move || model.with(|m| if reasoning { m.reasoning.clone() } else { m.streaming.clone() });
    if reasoning {
        view()
            .border_l(px(2.0))
            .border_l_color(ui::tok("border"))
            .pl(px(12.0))
            .pb(px(ROW_GAP))
            .child(
                text(body)
                    .italic()
                    .text_size(px(14.0))
                    .line_height(px(20.3))
                    .text_color(ui::tok("muted-foreground")),
            )
    } else {
        view()
            .border_l(px(2.0))
            .border_l_color(ui::tok("primary"))
            .pl(px(12.0))
            .pb(px(ROW_GAP))
            .child(
                text(body)
                    .text_size(px(16.0))
                    .line_height(px(24.0))
                    .text_color(ui::tok("foreground")),
            )
    }
}

/// A callback set the screen gives the dock: the composer's submit (Enter and
/// the send control share it) and the queue action (the draft, held).
pub struct Hooks {
    pub submit: Rc<dyn Fn(&str)>,
    pub queue: Rc<dyn Fn()>,
}

/// The composer dock (web): the status strip, the alerts and notes, the
/// attachments, the popovers, the decision sheet while one is pending, then
/// the composer with its actions. Pinned to the bottom; its top border
/// separates it from the transcript.
pub fn dock(
    model: Signal<Model>,
    draft: Signal<String>,
    nav: Signal<NavView>,
    actions: UnboundedSender<UiAction>,
    composer: Input,
    hooks: Hooks,
) -> impl IntoElement {
    let column = ui::gutter(
        view().flex_col().w_full().gap(px(8.0)).py(px(8.0)).max_w(px(ui::LIST_WIDTH)).self_center(),
    )
    .child(status_strip(model))
    .child(errors(model))
    .child(notes(model))
    .child(attachments(model, actions.clone()))
    .child(queue_popover(model, actions.clone()))
    .child(suggestions(draft, nav, actions.clone()))
    .child(when(move || model.with(|m| m.approval.is_some()), {
        let actions = actions.clone();
        move || approval_sheet(model, actions.clone())
    }))
    .child(when(move || model.with(|m| m.plan.is_some()), {
        let actions = actions.clone();
        move || plan_sheet(model, actions.clone())
    }))
    .child(ui::text_field("", "", composer))
    .child(composer_actions(model, draft, actions, hooks));
    view()
        .flex_col()
        .w_full()
        .bg(ui::tok("background"))
        .border_t_1()
        .border_color(ui::tok("border"))
        .child(column)
}

/// The status caption above a non-chat screen (web): the connection state
/// word and the status strip's facts on one muted line, so the status
/// surface stays in view outside the chat.
pub(crate) fn screen_status(model: Signal<Model>) -> impl IntoElement {
    ui::gutter(view().flex_col().w_full().pt(px(8.0))).child(
        text(move || {
            model.with(|m| {
                join_facts(&[first_fact(&m.connection), &m.provider, &m.running, &m.status_line])
            })
        })
        .text_size(px(13.0))
        .line_height(px(18.2))
        .text_color(ui::tok("muted-foreground")),
    )
}

/// The status strip: the provider, the status line (model, tokens, cache,
/// mode, phase, context), the standing goal and the hint, as one caption.
fn status_strip(model: Signal<Model>) -> impl IntoElement {
    text(move || {
        model.with(|m| {
            let run = format!("{} {}", m.running, m.spinner).trim().to_string();
            // The key hint for interrupting is the terminal's: here the
            // stop control says it (AGENTS.md divergence table, Input).
            let hint = if m.hint == modron_mobile_core::view::RUNNING_HINT { "" } else { &m.hint };
            join_facts(&[&run, &m.provider, &m.status_line, &m.goal, hint])
        })
    })
    .text_size(px(13.0))
    .line_height(px(18.2))
    .text_color(ui::tok("muted-foreground"))
}

/// The core's last error, as an alert.
fn errors(model: Signal<Model>) -> impl IntoElement {
    when(
        move || model.with(|m| m.error.is_some()),
        move || {
            ui::alert(Tone::Destructive, move || {
                model.with(|m| m.error.clone().unwrap_or_default())
            })
        },
    )
}

/// The core's notes (the last note, not the error, which is an alert).
fn notes(model: Signal<Model>) -> impl IntoElement {
    let note_at = move |i: usize| {
        model.with(|m| {
            m.notes.iter().filter(|n| m.error.as_deref() != Some(n.as_str())).nth(i).cloned()
        })
    };
    let mut column = view().flex_col().gap(px(4.0));
    for i in 0..NOTE_ROWS {
        column = column.child(when(
            move || note_at(i).is_some(),
            move || ui::caption(move || note_at(i).unwrap_or_default()),
        ));
    }
    column
}

/// The staged images: a chip per image and one remove control (the core's
/// `Detach`, which removes the last one, as Backspace on an empty composer
/// does).
fn attachments(model: Signal<Model>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    when(
        move || model.with(|m| !m.attachments.is_empty()),
        move || {
            let mut chips = view().flex_row().flex_wrap().gap(px(8.0)).items_center();
            for i in 0..ATTACH_ROWS {
                chips = chips.child(when(
                    move || model.with(|m| m.attachments.len() > i),
                    move || {
                        ui::badge(
                            move || {
                                model.with(|m| m.attachments.get(i).cloned().unwrap_or_default())
                            },
                            Tone::Muted,
                        )
                    },
                ));
            }
            let actions = actions.clone();
            chips.child(ui::button(REMOVE_LABEL, Variant::Ghost, Size::Default, move || {
                let _ = actions.send(UiAction::Detach);
            }))
        },
    )
}

/// The queued messages, one row each; a tap removes that message.
fn queue_popover(model: Signal<Model>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    when(
        move || model.with(|m| !m.queued.is_empty()),
        move || {
            let mut rows = view().flex_col();
            for i in 0..QUEUE_ROWS {
                let actions = actions.clone();
                rows = rows.child(when(
                    move || model.with(|m| m.queued.len() > i),
                    move || {
                        let actions = actions.clone();
                        ui::pick_row(
                            move || {
                                model.with(|m| {
                                    m.queued
                                        .get(i)
                                        .map(|q| queue_chip(i + 1, q))
                                        .unwrap_or_default()
                                })
                            },
                            move || {
                                let _ = actions.send(UiAction::Dequeue(i));
                            },
                        )
                    },
                ));
            }
            ui::card_list(rows)
        },
    )
}

/// The suggestions popover: command suggestions or `@file` paths, the
/// selected one marked, a window of POPOVER_ROWS around it. A tap completes
/// it. Esc hides it (the nav's `slash_hidden`).
fn suggestions(
    draft: Signal<String>,
    nav: Signal<NavView>,
    actions: UnboundedSender<UiAction>,
) -> impl IntoElement {
    let items = move || suggest::for_draft(&draft.get());
    let visible = move || !nav.with(|n| n.slash_hidden) && !items().names.is_empty();
    let start = move || {
        chat::slash_start(items().names.len(), nav.with(|n| n.slash_selected), POPOVER_ROWS)
    };
    when(visible, move || {
        let mut rows = view().flex_col();
        for i in 0..POPOVER_ROWS {
            let actions = actions.clone();
            rows = rows.child(when(
                move || visible() && items().names.len().min(POPOVER_ROWS) > i,
                move || {
                    let actions = actions.clone();
                    ui::pick_row(
                        move || {
                            let at = start() + i;
                            let selected = nav.with(|n| n.slash_selected) == at;
                            items()
                                .rows
                                .get(at)
                                .map(|row| {
                                    // The web collapses leading spaces: the
                                    // unselected rows keep the marker's width.
                                    if selected {
                                        chat::slash_row_label(row, true)
                                    } else {
                                        format!("\u{a0}\u{a0}{row}")
                                    }
                                })
                                .unwrap_or_default()
                        },
                        move || {
                            if let Some(name) = items().names.get(start() + i).cloned() {
                                let _ = actions.send(UiAction::Nav(NavStep::Complete(name)));
                            }
                        },
                    )
                },
            ));
        }
        ui::card_list(rows)
    })
}

/// The approval sheet: the tool and its summary, then the three decisions in
/// a row (allow once, allow all, deny). The type-ahead guard applies, as to
/// the keys: a press in the first 300 ms of a prompt decides nothing.
fn approval_sheet(model: Signal<Model>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    let option = move |i: usize| {
        let actions = actions.clone();
        let variant = match i {
            0 => Variant::Default,
            1 => Variant::Secondary,
            _ => Variant::Destructive,
        };
        ui::button_text(
            move || {
                model.with(|m| {
                    m.approval.as_ref().and_then(|a| a.labels.get(i).cloned()).unwrap_or_default()
                })
            },
            variant,
            Size::Default,
            move || {
                if chat::prompt_guard_open(Instant::now()) {
                    return;
                }
                let _ = actions.send(UiAction::Decide(i));
            },
        )
        .flex_1()
    };
    ui::card_list(
        view()
            .flex_col()
            .gap(px(4.0))
            .p(px(ui::GUTTER))
            .child(ui::card_title(move || {
                model.with(|m| m.approval.as_ref().map(|a| a.tool.clone()).unwrap_or_default())
            }))
            .child(ui::card_description(move || {
                model.with(|m| {
                    m.approval.as_ref().map(|a| a.args_summary.clone()).unwrap_or_default()
                })
            })),
    )
    .child(ui::card_footer(
        view().flex_row().gap(px(8.0)).w_full().child(option(0)).child(option(1)).child(option(2)),
    ))
}

/// The plan sheet: the proposed plan's lines, then the two answers (approve,
/// revise) with the core's labels. The same guard and clear as the keys.
fn plan_sheet(model: Signal<Model>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    let option = move |i: usize| {
        let actions = actions.clone();
        let variant = if i == 0 { Variant::Default } else { Variant::Outline };
        ui::button_text(
            move || {
                model.with(|m| {
                    m.plan.as_ref().and_then(|p| p.labels.get(i).cloned()).unwrap_or_default()
                })
            },
            variant,
            Size::Default,
            move || {
                if chat::prompt_guard_open(Instant::now()) {
                    return;
                }
                let action = chat::plan_answer(i == 0);
                chat::arm_plan_clear(&action);
                let _ = actions.send(action);
            },
        )
        .flex_1()
    };
    ui::card_list(
        view().flex_col().gap(px(8.0)).p(px(ui::GUTTER)).child(
            text(move || {
                model.with(|m| m.plan.as_ref().map(|p| p.lines.join("\n")).unwrap_or_default())
            })
            .text_size(px(15.0))
            .line_height(px(21.8))
            .text_color(ui::tok("foreground")),
        ),
    )
    .child(ui::card_footer(
        view().flex_row().gap(px(8.0)).w_full().child(option(0)).child(option(1)),
    ))
}

/// The composer's row under the field: queue and attach (ghost), and the
/// primary action: send while idle, stop while a turn runs.
fn composer_actions(
    model: Signal<Model>,
    draft: Signal<String>,
    actions: UnboundedSender<UiAction>,
    hooks: Hooks,
) -> impl IntoElement {
    let queue = hooks.queue.clone();
    let submit = hooks.submit.clone();
    let attach_actions = actions.clone();
    let stop_actions = actions;
    view()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .child(ui::button(QUEUE_LABEL, Variant::Ghost, Size::Default, move || queue()))
        .child(ui::button(chat::ATTACH_LABEL, Variant::Ghost, Size::Default, move || {
            chat::open_file_picker(attach_actions.clone())
        }))
        .child(view().flex_1())
        .child(when(
            move || !model.with(|m| m.is_running),
            move || {
                let submit = submit.clone();
                ui::button(SEND_LABEL, Variant::Default, Size::Default, move || {
                    submit(draft.get().as_str())
                })
                .disabled(move || draft.with(|d| d.trim().is_empty()))
            },
        ))
        .child(when(
            move || model.with(|m| m.is_running),
            move || {
                let stop_actions = stop_actions.clone();
                ui::button(STOP_LABEL, Variant::Outline, Size::Default, move || {
                    let _ = stop_actions.send(UiAction::Interrupt);
                })
            },
        ))
}

/// The height of a live text block of `text` at `line_px` per line, wrapped at
/// about `cols` characters, with its padding (0 when empty).
fn live_text_px(text: &str, line_px: f32, cols: usize) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    let lines: usize =
        text.split('\n').map(|line| line.chars().count().div_ceil(cols).max(1)).sum();
    lines.min(12) as f32 * line_px + ROW_GAP + 2.0
}

/// The transcript's height on the web: the window less the bar, the banner,
/// the live strips and the dock, which grows with its popovers and the
/// composer's lines. A floor keeps a sliver of transcript when everything
/// is open.
pub fn transcript_px(
    window: f32,
    model: Signal<Model>,
    nav: Signal<NavView>,
    draft: Signal<String>,
    details: bool,
) -> f32 {
    let (state, tool_panel, scrolled, live, pending, popovers) = (
        model.with(|m| first_fact(&m.connection).to_string()),
        nav.with(|n| n.tool_panel.is_some()),
        nav.with(|n| n.scroll_offset > 0),
        model.with(|m| (m.turn_summary.is_some(), m.tool_rows.len(), m.swarm_rows.len())),
        model.with(|m| {
            (
                m.approval.is_some(),
                m.plan.as_ref().map(|p| p.lines.len()).unwrap_or(0),
                m.queued.len(),
                m.attachments.len(),
                m.error.is_some(),
                m.notes.len(),
            )
        }),
        suggest::for_draft(&draft.get()).names.len().min(POPOVER_ROWS),
    );
    let lines = draft.with(|d| d.matches('\n').count() + 1).clamp(1, chat::COMPOSER_MAX_LINES);
    let (approval, plan_lines, queued, attached, error, notes) = pending;
    let mut reserve = BAR_H + DOCK_BASE_H + STRIP_H;
    if details {
        reserve += DETAILS_H;
    }
    reserve += banner_px(&state);
    if tool_panel {
        reserve += TOOL_PANEL_H;
    }
    // The live text sits between the transcript and the dock: its lines (the
    // streaming answer, the opted-in reasoning) come off the transcript too.
    let (streamed, reasoned) = model.with(|m| (m.streaming.clone(), m.reasoning.clone()));
    reserve += live_text_px(&streamed, 24.0, 44) + live_text_px(&reasoned, 20.0, 56);
    if scrolled {
        reserve += 20.0;
    }
    let (turn, tools, swarm) = live;
    if turn {
        reserve += STRIP_H + tools.min(chat::TOOL_SLOTS) as f32 * 18.0;
    }
    reserve += swarm.min(4) as f32 * 20.0;
    reserve += (lines - 1) as f32 * COMPOSER_LINE_PX;
    if error {
        reserve += 44.0;
    }
    reserve += notes.min(NOTE_ROWS) as f32 * 20.0;
    if attached > 0 {
        reserve += 52.0;
    }
    if queued > 0 {
        reserve += queued.min(QUEUE_ROWS) as f32 * POPOVER_ROW_H + 8.0;
    }
    if popovers > 0 {
        reserve += popovers as f32 * POPOVER_ROW_H + 8.0;
    }
    if approval {
        reserve += APPROVAL_H;
    }
    if plan_lines > 0 || model.with(|m| m.plan.is_some()) {
        reserve += PLAN_BASE_H + plan_lines as f32 * 20.0;
    }
    (window - reserve).max(MIN_TRANSCRIPT_H)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prose_without_fences_is_one_text_block() {
        assert_eq!(blocks("hello\n\nworld"), vec![Block::Text("hello\n\nworld".into())]);
    }

    #[test]
    fn a_fenced_block_splits_prose_and_code() {
        let raw = "Run this:\n```sh\ncargo test\n```\nThen read the output.";
        assert_eq!(
            blocks(raw),
            vec![
                Block::Text("Run this:".into()),
                Block::Code("cargo test".into()),
                Block::Text("Then read the output.".into()),
            ]
        );
    }

    #[test]
    fn an_open_fence_runs_to_the_end() {
        assert_eq!(blocks("```rs\nfn a() {}"), vec![Block::Code("fn a() {}".into())]);
    }

    #[test]
    fn indented_fences_still_count() {
        assert_eq!(blocks("  ```\n  x\n  ```"), vec![Block::Code("  x".into())]);
    }

    #[test]
    fn too_many_blocks_show_raw_so_nothing_drops() {
        let raw =
            (0..BLOCK_SLOTS + 2).map(|i| format!("```\n{i}\n```")).collect::<Vec<_>>().join("\n");
        assert_eq!(slots(&raw), vec![Block::Text(raw.clone())]);
    }

    #[test]
    fn the_first_fact_is_the_state_word() {
        assert_eq!(first_fact("connected \u{b7} ws://h/ws"), "connected");
        assert_eq!(first_fact("idle"), "idle");
    }

    #[test]
    fn join_facts_skips_the_empty_ones() {
        assert_eq!(
            join_facts(&["anthropic", "", "claude \u{b7} 0 tok", ""]),
            "anthropic \u{b7} claude \u{b7} 0 tok"
        );
    }
}

#[cfg(test)]
mod connection_tests {
    use super::*;

    #[test]
    fn a_dropped_link_is_a_warning_with_a_retry_and_a_closed_one_is_destructive() {
        assert_eq!(connection_tone("connected"), Tone::Success);
        assert_eq!(connection_tone("connecting"), Tone::Warning);
        assert_eq!(connection_tone(RECONNECTING_LABEL), Tone::Warning);
        assert_eq!(connection_tone("disconnected"), Tone::Destructive);
        assert_eq!(banner_px("connected"), 0.0);
        assert!(banner_px(RECONNECTING_LABEL) > banner_px("connecting"));
        assert!(banner_px("disconnected") > 0.0);
    }
}
