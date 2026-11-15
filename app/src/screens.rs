//! The screens around the chat body, target-neutral. [`frame`] draws the
//! status surface (connection, model, tokens, mode: on every screen), the
//! navigation controls, and the open screen. Keys are routed by
//! `chat::route_key`; taps send [`UiAction`]s. The words come from the core.

use modron_mobile_core::{
    FILTER_LABEL, NEW_SESSION_LABEL, SESSIONS_EMPTY_LABEL, SESSIONS_LABEL, SESSIONS_NO_MATCH_LABEL,
    SETTINGS_LABEL,
};
use nsh_core::element::AdaptiveColor;
use nsh_core::prelude::*;
use tokio::sync::mpsc::UnboundedSender;

use crate::chat::{self, style_token, token_ink, Model, Style, UiAction, WEB_CONTROLS};
use crate::nav::{NavStep, NavView, Screen};

/// Visible slots: fixed counts keep NSH's cell layout static (see the chat
/// body's notes). Rows past the slot count are not shown.
const SESSION_SLOTS: usize = 12;
const SETTING_SLOTS: usize = 24;

/// Terminal hints for the two openers. Key names are presentation (the
/// divergence table); the web shows buttons instead.
const TERMINAL_NAV_HINT: &str = "ctrl+o sessions   f2 settings";

fn muted() -> impl Fn() -> AdaptiveColor + Clone + 'static {
    || token_ink("muted-foreground")
}

/// The whole app screen: the status surface and navigation on top, the
/// open screen below it.
pub fn frame(
    model: Signal<Model>,
    draft: Signal<String>,
    nav: Signal<NavView>,
    actions: UnboundedSender<UiAction>,
) -> impl IntoElement {
    view()
        .flex_col()
        .size_full()
        .child(chat::header(model))
        .child(text(move || model.with(|m| m.status_line.clone())).text_color(muted()))
        .child(nav_bar(actions.clone()))
        .child(when(move || nav.with(|n| n.screen == Screen::Chat), {
            let actions = actions.clone();
            move || chat::screen(model, draft, actions.clone())
        }))
        .child(when(move || nav.with(|n| n.screen == Screen::Sessions), {
            let actions = actions.clone();
            move || sessions_body(nav, actions.clone())
        }))
        .child(when(move || nav.with(|n| n.screen == Screen::Settings), {
            let actions = actions.clone();
            move || settings_body(nav, actions.clone())
        }))
}

/// The two openers: pressable on the web, a hint line in the terminal.
fn nav_bar(actions: UnboundedSender<UiAction>) -> impl IntoElement {
    if !WEB_CONTROLS {
        return view().child(text(TERMINAL_NAV_HINT).text_color(muted()));
    }
    let opener = |label: &'static str, screen: Screen| {
        let actions = actions.clone();
        view()
            .role(AccessibilityRole::Button)
            .px(px(8.0))
            .on_press(move || {
                let _ = actions.send(UiAction::Nav(NavStep::Open(screen)));
            })
            .child(text(label).text_color(move || token_ink(style_token(Style::Plain))))
    };
    view()
        .flex_row()
        .child(opener(SESSIONS_LABEL, Screen::Sessions))
        .child(opener(SETTINGS_LABEL, Screen::Settings))
}

fn filter_line(filter: &str) -> String {
    format!("{FILTER_LABEL}: {filter}")
}

fn sessions_body(nav: Signal<NavView>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    let mut column = view().flex_col();
    column = column.child(text(move || nav.with(|n| filter_line(&n.filter))).text_color(muted()));
    column = column.child(when(
        move || nav.with(|n| n.total == 0),
        move || text(SESSIONS_EMPTY_LABEL).text_color(muted()),
    ));
    column = column.child(when(
        move || nav.with(|n| n.total > 0 && n.sessions.is_empty()),
        move || text(SESSIONS_NO_MATCH_LABEL).text_color(muted()),
    ));
    for i in 0..SESSION_SLOTS {
        let actions = actions.clone();
        column = column.child(when(
            move || nav.with(|n| n.sessions.len() > i),
            move || session_row(nav, i, actions.clone()),
        ));
    }
    column.child(new_session_control(actions))
}

/// One sessions row: the core's text, marked when selected. On the web a
/// tap attaches it.
fn session_row(
    nav: Signal<NavView>,
    i: usize,
    actions: UnboundedSender<UiAction>,
) -> impl IntoElement {
    let selected = move || nav.with(|n| n.selected == i);
    let line = move || {
        nav.with(|n| {
            let text = n.sessions.get(i).map(|r| r.text.clone()).unwrap_or_default();
            if n.selected == i {
                format!("> {text}")
            } else {
                text
            }
        })
    };
    let row = view().child(text(line).text_color(move || {
        token_ink(style_token(if selected() { Style::Bold } else { Style::Plain }))
    }));
    if WEB_CONTROLS {
        row.role(AccessibilityRole::Button).on_press(move || {
            if let Some(id) = nav.with(|n| n.sessions.get(i).map(|r| r.id)) {
                let _ = actions.send(UiAction::Nav(NavStep::Attach(id)));
            }
        })
    } else {
        row
    }
}

fn new_session_control(actions: UnboundedSender<UiAction>) -> impl IntoElement {
    let cell = view()
        .child(text(NEW_SESSION_LABEL).text_color(move || token_ink(style_token(Style::Plain))));
    if WEB_CONTROLS {
        cell.role(AccessibilityRole::Button).px(px(8.0)).on_press(move || {
            let _ = actions.send(UiAction::Nav(NavStep::NewSession));
        })
    } else {
        cell
    }
}

fn settings_body(nav: Signal<NavView>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    let mut column = view().flex_col();
    for i in 0..SETTING_SLOTS {
        let actions = actions.clone();
        column = column.child(when(
            move || nav.with(|n| n.settings.len() > i),
            move || setting_row(nav, i, actions.clone()),
        ));
    }
    column
}

/// One settings row: `label: value`. On the web a tap sends the row's
/// action (a read-only row sends nothing).
fn setting_row(
    nav: Signal<NavView>,
    i: usize,
    actions: UnboundedSender<UiAction>,
) -> impl IntoElement {
    let selected = move || nav.with(|n| n.selected == i);
    let line = move || {
        nav.with(|n| {
            let line = n.settings.get(i).map(|r| r.line()).unwrap_or_default();
            if n.selected == i {
                format!("> {line}")
            } else {
                line
            }
        })
    };
    let row = view().child(text(line).text_color(move || {
        token_ink(style_token(if selected() { Style::Bold } else { Style::Plain }))
    }));
    if WEB_CONTROLS {
        row.role(AccessibilityRole::Button).on_press(move || {
            let action = nav.with(|n| n.settings.get(i).and_then(|r| r.action.clone()));
            if let Some(action) = action {
                let _ = actions.send(action);
            }
        })
    } else {
        row
    }
}
