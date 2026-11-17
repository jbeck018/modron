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
        Variant::Outline => (Some("background"), "foreground", Some("border")),
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
    let mut root = view()
        .flex_col()
        .size_full()
        .bg(tok("background"))
        .pt_safe_area()
        .pl_safe_area()
        .pr_safe_area()
        .pb_safe_area();
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
