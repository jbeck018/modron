//! The web target's styled components (Program UX-1), in the shadcn way:
//! each one is a thin, open wrapper that takes its behaviour, state and
//! accessibility from an `nsh_base` component where one exists, and applies
//! this app's tokens and sizes. The rules are in `app/DESIGN.md`.
//!
//! From `nsh_base`: `Button` (press, disabled state, role). Composed here
//! from primitives, because `nsh_base` has no such component: Card,
//! Separator, Badge, Alert, Skeleton, Label, the form field, the header and
//! the list row. Text fields are NSH's own `input()` (the core element that
//! `nsh_base` re-exports), styled by [`text_field`].
//!
//! Every style applies only on the web (`WEB_CONTROLS`). On the terminal the
//! scaffold returns the body unchanged, so the terminal's projection is
//! unchanged.
//!
//! Colours are theme tokens only, read with [`tok`]; the radius is the
//! theme's `radius` token. Components:
//! - [`screen`] with [`Bar`]: header (back, title, actions), gutters, safe
//!   areas, content width.
//! - [`button`] (variants `Variant::{Default, Secondary, Outline, Ghost,
//!   Destructive, Link}`, sizes `Size::{Sm, Default, Lg, Icon}`), [`action`]
//!   (a Ghost header action).
//! - [`text_field`] (label, input, description), [`search_field`].
//! - [`card`], [`card_header`], [`card_title`], [`card_description`],
//!   [`card_content`], [`card_footer`], [`card_list`].
//! - [`separator`], [`badge`], [`alert`], [`skeleton`], [`caption`].
//! - [`list_row`], [`status_dot`], [`empty_state`].

use modron_mobile_core::view::BACK_LABEL;
use nsh_base::dialog;
use nsh_core::element::AnyElement;
use nsh_core::prelude::*;
use tokio::sync::mpsc::UnboundedSender;

use crate::chat::{token_ink, UiAction, WEB_CONTROLS};
use crate::nav::{NavStep, NavView};

/// Page gutter on phones; from [`WIDE_FROM`] points the wide gutter.
pub const GUTTER: f32 = 16.0;
pub const WIDE_GUTTER: f32 = 24.0;
pub const WIDE_FROM: f32 = 640.0;
/// Header bar height.
pub const HEADER_H: f32 = 52.0;
/// The smallest touch target (points); every control is at least this tall.
pub const TOUCH_H: f32 = 44.0;
/// Max content width: forms, and lists and chat.
pub const FORM_WIDTH: f32 = 400.0;
pub const LIST_WIDTH: f32 = 720.0;

/// A theme colour, read reactively: `.bg(tok("card"))`.
pub fn tok(name: &'static str) -> impl Fn() -> AdaptiveColor + Clone + 'static {
    move || token_ink(name)
}

/// The theme's corner radius (the `radius` token; NSH's default when the
/// theme names none).
pub fn radius() -> f32 {
    tokens().number("radius").unwrap_or(12.0)
}

/// The page gutter for the window's width (16 on phones, 24 from 640).
pub fn gutter<V: Styled>(v: V) -> V {
    v.px(px(GUTTER)).style_when(|| window().width >= px(WIDE_FROM), |s| s.px(px(WIDE_GUTTER)))
}

/// The body text size: 16 on phones, 15 on wide windows.
fn body_size<T: Styled>(t: T) -> T {
    t.text_size(px(16.0)).style_when(|| window().width >= px(WIDE_FROM), |s| s.text_size(px(15.0)))
}

/// A caption: 13 pt, muted (shadcn's `text-sm text-muted-foreground`).
pub fn caption(line: impl IntoReactive<SharedString>) -> Text {
    text(line).text_size(px(13.0)).text_color(tok("muted-foreground"))
}

/// A title: 17 pt semibold.
fn title_text(line: impl IntoReactive<SharedString>) -> Text {
    text(line).text_size(px(17.0)).font_weight(FontWeight::SEMIBOLD).text_color(tok("foreground"))
}

/// The button variants (shadcn's set).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Variant {
    /// Filled with the primary token.
    Default,
    /// Filled with the secondary token.
    Secondary,
    /// A background with a border.
    Outline,
    /// Text only.
    Ghost,
    /// Filled with the destructive token.
    Destructive,
    /// Text in the link token, no fill.
    Link,
}

/// The button sizes (shadcn's set). Every size is at least 44 pt tall.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Size {
    Sm,
    Default,
    Lg,
    /// A square 44 pt target for an icon.
    Icon,
}

/// A button: `nsh_base`'s `Button` (press, disabled and accessibility) with
/// this app's variant and size. The label is its accessible name. Chain
/// `.w_full()` for full width and `.disabled(flag)` to disable it.
pub fn button(
    label: &'static str,
    variant: Variant,
    size: Size,
    press: impl Fn() + 'static,
) -> nsh_base::ButtonElement {
    button_text(label, variant, size, press)
}

/// [`button`] with a reactive label, for words the core supplies at run time
/// (an approval's or a plan's option labels).
pub fn button_text(
    label: impl IntoReactive<SharedString>,
    variant: Variant,
    size: Size,
    press: impl Fn() + 'static,
) -> nsh_base::ButtonElement {
    let (fill, ink, edge) = match variant {
        Variant::Default => (Some("primary"), "primary-foreground", None),
        Variant::Secondary => (Some("secondary"), "secondary-foreground", None),
        // muted-foreground, not border: the border and input tokens are
        // #2A2A2A in dark, which vanished on a card. muted-foreground is
        // #ADADAD dark and #5F5F5F light: visible on both page and card.
        Variant::Outline => (Some("background"), "foreground", Some("muted-foreground")),
        Variant::Ghost => (None, "foreground", None),
        Variant::Destructive => (Some("destructive"), "destructive-foreground", None),
        Variant::Link => (None, "link", None),
    };
    let (height, pad, font) = match size {
        Size::Sm => (TOUCH_H, 12.0, 15.0),
        Size::Default => (TOUCH_H, 16.0, 16.0),
        Size::Lg => (52.0, 24.0, 17.0),
        Size::Icon => (TOUCH_H, 0.0, 16.0),
    };
    let mut control = nsh_base::button()
        .flex_row()
        .items_center()
        .justify_center()
        .min_h(px(height))
        .min_w(px(if size == Size::Icon { height } else { 0.0 }))
        .px(px(pad))
        .rounded(px(radius()))
        .cursor_pointer()
        .active(|s| s.opacity(0.75))
        .disabled_style(|s| s.opacity(0.4))
        .focus_visible(|s| s.ring(px(2.0)).ring_color(tok("ring")));
    if let Some(fill) = fill {
        control = control.bg(tok(fill));
    }
    control = match variant {
        Variant::Ghost | Variant::Link => control.hover(|s| s.bg(tok("accent"))),
        Variant::Outline => control.hover(|s| s.bg(tok("accent"))),
        Variant::Secondary => control.hover(|s| s.opacity(0.9)),
        Variant::Default | Variant::Destructive => control.hover(|s| s.opacity(0.92)),
    };
    if let Some(edge) = edge {
        control = control.border_1().border_color(tok(edge));
    }
    control
        .child(
            text(label)
                .text_size(px(font))
                .font_weight(match variant {
                    Variant::Ghost | Variant::Link => FontWeight::MEDIUM,
                    _ => FontWeight::SEMIBOLD,
                })
                .text_color(tok(ink)),
        )
        .on_press(press)
}

/// A Ghost button that sends `step` through `tx` on press (header actions).
pub fn action(
    label: &'static str,
    tx: UnboundedSender<UiAction>,
    step: UiAction,
) -> impl IntoElement {
    button(label, Variant::Ghost, Size::Default, move || {
        let _ = tx.send(step.clone());
    })
}

/// A label above a control, then the control, then a description line. `label`
/// and `description` may be "" to leave them out.
pub fn text_field(label: &'static str, description: &'static str, control: Input) -> View {
    let field = view()
        .flex_row()
        .items_center()
        .min_h(px(TOUCH_H))
        .px(px(12.0))
        .rounded(px(radius()))
        .border_1()
        .border_color(tok("input"))
        .bg(tok("background"))
        .focus_within(|s| s.ring(px(2.0)).ring_color(tok("ring")))
        .child(
            control
                .flex_1()
                .text_size(px(16.0))
                .text_color(tok("foreground"))
                .placeholder_color(move || nsh_core::element::color(token_ink("muted-foreground"))),
        );
    let mut column = view().flex_col().gap(px(8.0));
    if !label.is_empty() {
        column = column.child(caption(label).font_weight(FontWeight::MEDIUM));
    }
    column = column.child(field);
    if !description.is_empty() {
        column = column.child(caption(description));
    }
    column
}

/// A display field for a typed filter: the value, or `placeholder` in muted
/// words while it is empty. The keys are the page's (typing filters).
pub fn search_field(
    value: impl Fn() -> String + Clone + 'static,
    placeholder: &'static str,
) -> View {
    let shown = value.clone();
    let tint = value;
    view()
        .flex_row()
        .items_center()
        .min_h(px(TOUCH_H))
        .px(px(12.0))
        .rounded(px(radius()))
        .border_1()
        .border_color(tok("input"))
        .bg(tok("background"))
        .child(
            body_size(text(move || {
                let value = shown();
                if value.is_empty() {
                    placeholder.to_string()
                } else {
                    value
                }
            }))
            .text_color(move || {
                if tint().is_empty() {
                    token_ink("muted-foreground")
                } else {
                    token_ink("foreground")
                }
            }),
        )
}

/// A card: the card token, a 1 pt border, the radius, no shadow. Its
/// children are its header, content and footer (or any element).
pub fn card(children: impl IntoElement) -> View {
    card_list(children)
}

/// A card with no padding of its own, for rows (its corners clip them).
pub fn card_list(children: impl IntoElement) -> View {
    view()
        .flex_col()
        .bg(tok("card"))
        .border_1()
        .border_color(tok("border"))
        .rounded(px(radius()))
        .overflow_hidden()
        .child(children)
}

/// The card's header: 16 pt padded, a column with 4 pt between its lines.
pub fn card_header(children: impl IntoElement) -> View {
    view().flex_col().gap(px(4.0)).p(px(GUTTER)).child(children)
}

/// The card's title: 17 pt semibold.
pub fn card_title(line: impl IntoReactive<SharedString>) -> Text {
    title_text(line)
}

/// The card's description: 13 pt, muted.
pub fn card_description(line: impl IntoReactive<SharedString>) -> Text {
    caption(line)
}

/// The card's content: 16 pt padded on every side, a column with 16 pt
/// between its children.
pub fn card_content(children: impl IntoElement) -> View {
    view().flex_col().gap(px(16.0)).p(px(GUTTER)).child(children)
}

/// The card's footer: a row of actions, 16 pt padded, above a 1 pt border.
pub fn card_footer(children: impl IntoElement) -> View {
    view()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .p(px(GUTTER))
        .border_t_1()
        .border_color(tok("border"))
        .child(children)
}

/// A 1 pt line in the border token.
pub fn separator() -> View {
    view().h(px(1.0)).w_full().bg(tok("border"))
}

/// The tones of a status dot, badge or alert.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Success,
    Muted,
    Destructive,
    Warning,
}

fn tone_token(tone: Tone) -> &'static str {
    match tone {
        Tone::Success => "success",
        Tone::Muted => "muted-foreground",
        Tone::Destructive => "destructive",
        Tone::Warning => "warning",
    }
}

/// A small pill that names a status (shadcn's Badge, outline form): the
/// tone's colour for the text and the border.
pub fn badge(label: impl IntoReactive<SharedString>, tone: Tone) -> View {
    view()
        .flex_row()
        .items_center()
        .px(px(8.0))
        .py(px(2.0))
        .rounded_full()
        .border_1()
        .border_color(move || token_ink(tone_token(tone)))
        .child(
            text(label)
                .text_size(px(12.0))
                .font_weight(FontWeight::MEDIUM)
                .text_color(move || token_ink(tone_token(tone))),
        )
}

/// An 8 pt dot in the tone's colour (reactive: `tone` is read each time).
pub fn status_dot(tone: impl Fn() -> Tone + 'static) -> View {
    view().w(px(8.0)).h(px(8.0)).rounded_full().bg(move || token_ink(tone_token(tone())))
}

/// A bordered line in `tone`'s colour (shadcn's Alert, for a refusal, a
/// warning or a result).
pub fn alert(tone: Tone, line: impl IntoReactive<SharedString>) -> View {
    view()
        .flex_row()
        .items_center()
        .px(px(12.0))
        .py(px(8.0))
        .rounded(px(radius()))
        .border_1()
        .border_color(move || token_ink(tone_token(tone)))
        .child(body_size(text(line)).text_color(move || token_ink(tone_token(tone))).flex_1())
}

/// A placeholder block for content that is loading (shadcn's Skeleton): a
/// muted, rounded bar `height` points tall, full width.
pub fn skeleton(height: f32) -> View {
    view().w_full().h(px(height)).rounded(px(radius())).bg(tok("muted"))
}

/// A list row: a title and a muted subtitle, `trailing` at the end, a 52 pt
/// full-row press target with hover, pressed and focus states, and a
/// separator under it. Chain `.disabled(flag)` to stop presses, or dim it
/// with `.style_when(..)` when the navigation decides on the press.
pub fn list_row(
    title: impl IntoReactive<SharedString>,
    subtitle: impl IntoReactive<SharedString>,
    trailing: Option<AnyElement>,
    press: impl Fn() + 'static,
) -> View {
    let lines = view()
        .flex_col()
        .flex_1()
        .gap(px(2.0))
        .child(body_size(text(title)).text_color(tok("foreground")).truncate())
        .child(caption(subtitle).truncate());
    let mut row = view()
        .role(AccessibilityRole::Button)
        .flex_row()
        .items_center()
        .gap(px(12.0))
        .min_h(px(52.0))
        .px(px(GUTTER))
        .py(px(8.0))
        .border_b_1()
        .border_color(tok("border"))
        .cursor_pointer()
        .hover(|s| s.bg(tok("accent")))
        .active(|s| s.bg(tok("muted")))
        .disabled_style(|s| s.opacity(0.5))
        .focus_visible(|s| s.ring(px(2.0)).ring_color(tok("ring")))
        .child(lines);
    if let Some(trailing) = trailing {
        row = row.child(trailing);
    }
    row.on_press(press)
}

/// A one-line row (a queued message, a suggestion): the title in the row's
/// 52 pt press target, with no subtitle line. [`list_row`] keeps its subtitle
/// line even when it is empty, which left these rows top-heavy.
pub fn pick_row(title: impl IntoReactive<SharedString>, press: impl Fn() + 'static) -> View {
    view()
        .role(AccessibilityRole::Button)
        .flex_row()
        .items_center()
        .min_h(px(52.0))
        .px(px(GUTTER))
        .py(px(8.0))
        .border_b_1()
        .border_color(tok("border"))
        .cursor_pointer()
        .hover(|s| s.bg(tok("accent")))
        .active(|s| s.bg(tok("muted")))
        .disabled_style(|s| s.opacity(0.5))
        .focus_visible(|s| s.ring(px(2.0)).ring_color(tok("ring")))
        .child(body_size(text(title)).text_color(tok("foreground")).flex_1().truncate())
        .on_press(press)
}

/// Centred words for an empty list: a title, an optional line (pass "" to
/// leave it out) and an optional action.
pub fn empty_state(title: &'static str, line: &'static str, action: Option<AnyElement>) -> View {
    let mut column =
        view().flex_col().items_center().gap(px(8.0)).py(px(32.0)).child(title_text(title));
    if !line.is_empty() {
        column = column.child(caption(line).text_center());
    }
    if let Some(action) = action {
        column = column.child(action);
    }
    column
}

/// The web's welcome for a blank chat: the title over the tagline and the
/// model line, centred, in the empty state's layout. It carries no key hints
/// (the terminal's keys are not the web's controls).
pub fn welcome_state(
    title: impl IntoReactive<SharedString>,
    tagline: &'static str,
    model: impl IntoReactive<SharedString>,
) -> View {
    view()
        .flex_col()
        .items_center()
        .gap(px(8.0))
        .py(px(32.0))
        .child(title_text(title).text_center())
        .child(caption(tagline).text_center())
        .child(caption(model).text_center())
}

/// A section title over a card (shadcn's Label): 13 pt, medium, muted.
pub fn section_title(line: &'static str) -> Text {
    caption(line).font_weight(FontWeight::MEDIUM)
}

/// Body text in the foreground token, at the page's body size.
pub fn body(line: impl IntoReactive<SharedString>) -> Text {
    body_size(text(line)).text_color(tok("foreground"))
}

/// A row's two words: the label on the left, the value on the right (muted).
pub fn value_pair(
    label: impl IntoReactive<SharedString>,
    value: impl IntoReactive<SharedString>,
) -> View {
    view()
        .flex_row()
        .items_center()
        .gap(px(12.0))
        .flex_1()
        .child(body(label).flex_1().truncate())
        .child(
            body_size(text(value)).text_color(tok("muted-foreground")).max_w(px(180.0)).truncate(),
        )
}

/// A row's trailing control, in its slot: the control at the row's end, with
/// 8 pt to the card's edge while `shown` holds. A hidden control takes no
/// width, so the row's press fill reaches the card's edge (the slot is the
/// only place the inset lives; pass `true` for a control that is always
/// there). Use it as the `trailing` of [`row`] or [`static_row`].
pub fn trailing(control: impl IntoElement, shown: impl IntoReactive<bool>) -> AnyElement {
    view()
        .flex_row()
        .items_center()
        .style_when(shown, |s| s.pr(px(8.0)))
        .child(control)
        .into_element()
}

/// The row's bar: the content at the page gutter, 52 pt at least, with a
/// 1 pt line under it and an optional trailing slot (see [`trailing`]; it
/// keeps its own touch target).
fn row_bar(main: View, trailing: Option<AnyElement>) -> View {
    let mut bar = view()
        .flex_row()
        .items_center()
        .min_h(px(52.0))
        .border_b_1()
        .border_color(tok("border"))
        .child(main);
    if let Some(trailing) = trailing {
        bar = bar.child(trailing);
    }
    bar
}

fn row_main(content: impl IntoElement) -> View {
    view()
        .flex_row()
        .items_center()
        .gap(px(12.0))
        .flex_1()
        .min_h(px(52.0))
        .px(px(GUTTER))
        .py(px(8.0))
        .child(content)
}

/// A pressable list row: the whole content is the press target (hover, pressed
/// and focus states), `trailing` is its own control beside it.
pub fn row(
    content: impl IntoElement,
    trailing: Option<AnyElement>,
    press: impl Fn() + 'static,
) -> View {
    let main = row_main(content)
        .role(AccessibilityRole::Button)
        .cursor_pointer()
        .hover(|s| s.bg(tok("accent")))
        .active(|s| s.bg(tok("muted")))
        .focus_visible(|s| s.ring(px(2.0)).ring_color(tok("ring")))
        .on_press(press);
    row_bar(main, trailing)
}

/// A row that does not press (its content is read-only, or its control is).
pub fn static_row(content: impl IntoElement, trailing: Option<AnyElement>) -> View {
    row_bar(row_main(content), trailing)
}

/// A picker row: the current value carries `mark` (a dot) before its label,
/// an unavailable row is dim, and the keyboard's selected row is tinted.
pub fn choice_row(
    mark: &'static str,
    current: impl Fn() -> bool + 'static,
    selected: impl Fn() -> bool + 'static,
    dim: impl Fn() -> bool + 'static,
    label: impl IntoReactive<SharedString>,
    press: impl Fn() + 'static,
) -> View {
    let marked = text(move || if current() { mark.to_string() } else { String::new() })
        .text_size(px(16.0))
        .text_color(tok("primary"))
        .w(px(16.0));
    let title = body_size(text(label))
        .text_color(
            move || {
                if dim() {
                    token_ink("muted-foreground")
                } else {
                    token_ink("foreground")
                }
            },
        )
        .flex_1()
        .truncate();
    let content = view().flex_row().items_center().gap(px(8.0)).flex_1().child(marked).child(title);
    row(content, None, press).style_when(selected, |s| s.bg(tok("accent")))
}

/// A switch with its label for screen readers (NSH's own on / off control,
/// drawn as a 44 x 24 track). `on_change` runs on each press; the switch
/// shows the value `checked` gives back.
pub fn switch(
    checked: impl Fn() -> bool + 'static,
    disabled: impl Fn() -> bool + 'static,
    label: &'static str,
    on_change: impl Fn() + 'static,
) -> impl IntoElement {
    nsh_base::switch()
        .checked(checked)
        .disabled(disabled)
        .accessibility_label(Some(SharedString::from(label)))
        .hit_slop(EdgeInsets::all(px(10.0)))
        .track_style(|s| {
            s.w(px(44.0))
                .h(px(24.0))
                .rounded_full()
                .bg(token_ink("input"))
                .checked(|s| s.bg(token_ink("primary")))
        })
        .thumb_style(|s| {
            s.w(px(20.0))
                .h(px(20.0))
                .rounded_full()
                .bg(token_ink("background"))
                .ml(px(2.0))
                .checked(|s| s.ml(px(22.0)))
        })
        .on_change(move |_: &bool| on_change())
        .child(view())
}

/// The tab list (a segmented control over `tabs`): a muted track, the tabs
/// in one row.
pub fn tab_list(tabs: impl IntoElement) -> View {
    view()
        .flex_row()
        .p(px(4.0))
        .rounded(px(radius()))
        .bg(tok("muted"))
        .child(nsh_base::tabs().flex_row().flex_1().child(tabs))
}

/// One tab of a [`tab_list`]: the label, the selected tab on the background
/// token, a 44 pt target, a press that selects it.
pub fn tab(
    label: &'static str,
    selected: impl Fn() -> bool + Clone + 'static,
    press: impl Fn() + 'static,
) -> impl IntoElement {
    let ink = selected.clone();
    nsh_base::tab()
        .selected(selected)
        .on_press(move |_: &()| press())
        .flex_1()
        .min_h(px(TOUCH_H))
        .items_center()
        .justify_center()
        .rounded(px(radius()))
        .cursor_pointer()
        .selected_style(|s| s.bg(token_ink("background")))
        .focus_visible(|s| s.ring(px(2.0)).ring_color(tok("ring")))
        .child(
            text(label)
                .text_size(px(15.0))
                .font_weight(FontWeight::MEDIUM)
                .text_color(move || {
                    if ink() {
                        token_ink("foreground")
                    } else {
                        token_ink("muted-foreground")
                    }
                })
                .truncate(),
        )
}

/// A labelled display field for an inline form: the label over a bordered
/// box with the value (or nothing), ringed while it is the active field.
pub fn field(
    label: impl IntoReactive<SharedString>,
    value: impl Fn() -> String + 'static,
    active: impl Fn() -> bool + 'static,
) -> View {
    view().flex_col().gap(px(8.0)).child(caption(label).font_weight(FontWeight::MEDIUM)).child(
        view()
            .flex_row()
            .items_center()
            .min_h(px(TOUCH_H))
            .px(px(12.0))
            .rounded(px(radius()))
            .border_1()
            .border_color(tok("input"))
            .bg(tok("background"))
            .style_when(active, |s| s.ring(px(2.0)).ring_color(tok("ring")))
            .child(body(value)),
    )
}

/// A destructive confirmation as a modal dialog (nsh_base's `dialog`, the
/// platform's modal layer): the question as its title, `description` below it
/// (`""` leaves it out), then the cancel button (outline) and the confirm
/// button (destructive). Escape and the back control send `on_cancel`; the
/// platform keeps the focus inside the dialog and returns it when it closes.
/// Present it by putting it in the view; leave it out to close it.
pub fn confirm_dialog(
    title: impl IntoReactive<SharedString>,
    description: &'static str,
    confirm_label: &'static str,
    cancel_label: &'static str,
    on_confirm: impl Fn() + 'static,
    on_cancel: impl Fn() + Clone + 'static,
) -> impl IntoElement {
    let cancel = on_cancel.clone();
    let mut header = view().flex_col().gap(px(4.0)).p(px(GUTTER)).child(card_title(title));
    if !description.is_empty() {
        header = header.child(card_description(description));
    }
    let footer = card_footer(
        button(cancel_label, Variant::Outline, Size::Default, cancel).flex_1().into_element(),
    )
    .child(
        button(confirm_label, Variant::Destructive, Size::Default, on_confirm)
            .flex_1()
            .into_element(),
    );
    let card = card_list(view().flex_col().child(header).child(footer));
    dialog().on_cancel(on_cancel).child(
        view()
            .flex_col()
            .size_full()
            .justify_center()
            .items_center()
            .p(px(GUTTER))
            .bg(scrim)
            .child(view().flex_col().w_full().max_w(px(FORM_WIDTH)).child(card)),
    )
}

/// The dimming behind a dialog: the background token at three quarters, so
/// it darkens in dark appearance and washes out in light.
fn scrim() -> AdaptiveColor {
    let c = token_ink("background");
    AdaptiveColor { light: c.light.alpha(0.75), dark: c.dark.alpha(0.75) }
}

/// The scaffold's header and its content, in one value. `Bar::new(title)`
/// makes a titled screen; `Bar::blank()` shows no header (sign-in).
pub struct Bar {
    title: Option<AnyElement>,
    back: Option<AnyElement>,
    actions: Vec<AnyElement>,
    narrow: bool,
}

impl Bar {
    /// A titled screen (the title is the screen's own word, reactive if
    /// it must be).
    pub fn new(title: impl IntoReactive<SharedString>) -> Self {
        Bar {
            title: Some(title_text(title).truncate().flex_1().into_element()),
            back: None,
            actions: Vec::new(),
            narrow: false,
        }
    }

    /// No header; the body is centred (sign-in).
    pub fn blank() -> Self {
        Bar { title: None, back: None, actions: Vec::new(), narrow: true }
    }

    /// The content column is the form width (400) rather than the list width.
    pub fn form(mut self) -> Self {
        self.narrow = true;
        self
    }

    /// The back control: what Esc does on this screen (close the confirmation,
    /// the form, the Cloud token entry, the filter, else back one screen).
    pub fn back(mut self, nav: Signal<NavView>, tx: UnboundedSender<UiAction>) -> Self {
        let press = move || {
            let step = nav.with(|n| {
                if n.confirm.is_some() {
                    NavStep::Cancel
                } else if n.form.is_some() {
                    NavStep::FormCancel
                } else if matches!(
                    n.screen,
                    crate::nav::Screen::Cloud | crate::nav::Screen::CloudRepos
                ) {
                    NavStep::CloudEsc
                } else if !n.filter.is_empty() {
                    NavStep::ClearFilter
                } else {
                    NavStep::Back
                }
            });
            let _ = tx.send(UiAction::Nav(step));
        };
        self.back = Some(button(BACK_LABEL, Variant::Ghost, Size::Default, press).into_element());
        self
    }

    /// A header action at the end of the bar (a Ghost button, or a `when` of
    /// one for a control that shows only sometimes).
    pub fn action(mut self, element: impl IntoElement) -> Self {
        self.actions.push(element.into_element());
        self
    }
}

/// The screen scaffold: on the web, a header (back, title, actions) with a
/// border under it, then the body in a scroll view with the page gutter and
/// the content width, inside the safe areas. On the terminal, `body` alone.
pub fn screen(bar: Bar, body: impl IntoElement) -> AnyElement {
    if !WEB_CONTROLS {
        return body.into_element();
    }
    let width = if bar.narrow { FORM_WIDTH } else { LIST_WIDTH };
    // The header's content sits in the same column as the body: on a wide
    // window the border spans the page, the controls line up with the list.
    let content =
        gutter(view().flex_col().w_full().max_w(px(width)).self_center()).py(px(16.0)).child(body);
    let main = match bar.title {
        Some(_) => scroll().flex_1().child(content).into_element(),
        None => view().flex_1().flex_col().justify_center().child(content).into_element(),
    };
    // A titled screen takes the height the header and the nav row leave (the
    // frame stacks them above it); a blank one fills the window.
    let mut root = view()
        .flex_col()
        .bg(tok("background"))
        .pt_safe_area()
        .pl_safe_area()
        .pr_safe_area()
        .pb_safe_area();
    root = if bar.title.is_some() { root.flex_1().w_full() } else { root.size_full() };
    if let Some(title) = bar.title {
        let mut row = gutter(view().flex_row().items_center().gap(px(4.0)).w_full())
            .min_h(px(HEADER_H))
            .max_w(px(width))
            .self_center();
        if let Some(back) = bar.back {
            row = row.child(back);
        }
        row = row.child(title);
        for action in bar.actions {
            row = row.child(action);
        }
        let header = view().flex_col().w_full().border_b_1().border_color(tok("border")).child(row);
        root = root.child(header);
    }
    root.child(main).into_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_touch_target_is_at_least_44_points() {
        assert_eq!(TOUCH_H, 44.0);
        assert_eq!(HEADER_H, 52.0);
    }

    #[test]
    fn the_gutters_follow_the_design_standard() {
        assert_eq!(GUTTER, 16.0);
        assert_eq!(WIDE_GUTTER, 24.0);
        assert_eq!(WIDE_FROM, 640.0);
        assert_eq!(FORM_WIDTH, 400.0);
        assert_eq!(LIST_WIDTH, 720.0);
    }

    #[test]
    fn every_tone_has_a_theme_token() {
        for tone in [Tone::Success, Tone::Muted, Tone::Destructive, Tone::Warning] {
            assert!(!tone_token(tone).is_empty());
        }
    }
}
