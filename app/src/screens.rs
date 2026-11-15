//! The screens around the chat body, target-neutral. [`frame`] draws the
//! status surface (connection, model, tokens, mode: on every screen), the
//! navigation controls, and the open screen. Keys are routed by
//! `chat::route_key`; taps send [`UiAction`]s. The words come from the core
//! or the shared table.

use modron_mobile_core::view::{
    AGENT_ENDED_LABEL, AGENT_KILL_LABEL, AGENT_NONE_LABEL, CONFIRM_NO_LABEL, CONFIRM_YES_LABEL,
    DELETE_LABEL, HELP_COMMANDS_HEADING, HELP_KEYS_HEADING, HELP_TITLE, MODELS_EMPTY_LABEL,
};
use modron_mobile_core::{
    FILTER_LABEL, NEW_SESSION_LABEL, SESSIONS_EMPTY_LABEL, SESSIONS_LABEL, SESSIONS_NO_MATCH_LABEL,
    SETTINGS_LABEL,
};
use nsh_core::element::AdaptiveColor;
use nsh_core::prelude::*;
use tokio::sync::mpsc::UnboundedSender;

use crate::chat::{self, style_token, token_ink, Model, Style, UiAction, WEB_CONTROLS};
use crate::help;
use crate::nav::{NavStep, NavView, Screen};

/// Visible slots: fixed counts keep NSH's cell layout static (see the chat
/// body's notes). Rows past the slot count are not shown.
const SESSION_SLOTS: usize = 12;
const SETTING_SLOTS: usize = 24;
const MODEL_SLOTS: usize = 16;

/// Terminal hints for the openers. Key names are presentation (the
/// divergence table); the web shows buttons instead.
const TERMINAL_NAV_HINT: &str = "ctrl+o sessions   f2 settings   f1 help";

fn muted() -> impl Fn() -> AdaptiveColor + Clone + 'static {
    || token_ink("muted-foreground")
}

/// The whole app screen: the status surface and navigation on top, the
/// open screen below it.
pub fn frame(
    model: Signal<Model>,
    draft: Signal<String>,
    nav: Signal<NavView>,
    window_h: Signal<f32>,
    actions: UnboundedSender<UiAction>,
) -> impl IntoElement {
    view()
        .flex_col()
        .size_full()
        .child(chat::header(model))
        .child(text(move || model.with(|m| m.status_line.clone())).text_color(muted()))
        .child(nav_bar(actions.clone()))
        .child(when(move || nav.with(|n| matches!(n.screen, Screen::Chat | Screen::Agent)), {
            let actions = actions.clone();
            move || chat::screen(model, draft, nav, window_h, actions.clone())
        }))
        .child(when(move || nav.with(|n| n.screen == Screen::Sessions), {
            let actions = actions.clone();
            move || sessions_body(nav, actions.clone())
        }))
        .child(when(move || nav.with(|n| n.screen == Screen::Settings), {
            let actions = actions.clone();
            move || settings_body(nav, actions.clone())
        }))
        .child(when(move || nav.with(|n| n.screen == Screen::Models), {
            let actions = actions.clone();
            move || models_body(nav, actions.clone())
        }))
        .child(when(move || nav.with(|n| n.screen == Screen::Help), help_body))
}

/// The openers: pressable on the web, a hint line in the terminal.
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
        .child(opener(HELP_TITLE, Screen::Help))
}

/// A pressable word (web): one button that sends `action`.
fn tap(
    label: &'static str,
    actions: UnboundedSender<UiAction>,
    action: UiAction,
) -> impl IntoElement {
    view()
        .role(AccessibilityRole::Button)
        .px(px(8.0))
        .on_press(move || {
            let _ = actions.send(action.clone());
        })
        .child(text(label).text_color(move || token_ink(style_token(Style::Plain))))
}

/// The inline confirmation: the shared line, and on the web the yes and no
/// buttons. Shown only while a confirmation waits.
fn confirm_line(nav: Signal<NavView>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    when(
        move || nav.with(|n| n.confirm.is_some()),
        move || {
            let yes = actions.clone();
            let no = actions.clone();
            let mut row = view().flex_row().child(
                text(move || nav.with(|n| n.confirm.clone().unwrap_or_default()))
                    .text_color(move || token_ink("warning")),
            );
            if WEB_CONTROLS {
                row = row
                    .child(tap(CONFIRM_YES_LABEL, yes, UiAction::Nav(NavStep::Confirm)))
                    .child(tap(CONFIRM_NO_LABEL, no, UiAction::Nav(NavStep::Cancel)));
            }
            row
        },
    )
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
    column.child(new_session_control(actions.clone())).child(confirm_line(nav, actions))
}

/// One sessions row: the core's text, marked when selected. On the web a
/// tap attaches it, and its delete control asks first.
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
        let attach = actions.clone();
        let attach_row = row.role(AccessibilityRole::Button).on_press(move || {
            if let Some(id) = nav.with(|n| n.sessions.get(i).map(|r| r.id)) {
                let _ = attach.send(UiAction::Nav(NavStep::Attach(id)));
            }
        });
        view().flex_row().child(attach_row).child(tap(
            DELETE_LABEL,
            actions,
            UiAction::Nav(NavStep::AskDelete(i)),
        ))
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

/// The model picker: the filter line, the rows (`model (role)`), and the
/// empty state. Enter (terminal) or a tap (web) switches the model.
fn models_body(nav: Signal<NavView>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    let mut column = view().flex_col();
    column = column.child(text(move || nav.with(|n| filter_line(&n.filter))).text_color(muted()));
    column = column.child(when(
        move || nav.with(|n| n.models_total == 0),
        move || text(MODELS_EMPTY_LABEL).text_color(muted()),
    ));
    for i in 0..MODEL_SLOTS {
        let actions = actions.clone();
        column = column.child(when(
            move || nav.with(|n| n.models.len() > i),
            move || model_row(nav, i, actions.clone()),
        ));
    }
    column
}

fn model_row(
    nav: Signal<NavView>,
    i: usize,
    actions: UnboundedSender<UiAction>,
) -> impl IntoElement {
    let selected = move || nav.with(|n| n.selected == i);
    let line = move || {
        nav.with(|n| {
            let label = n.models.get(i).map(|r| r.text.clone()).unwrap_or_default();
            if n.selected == i {
                format!("> {label}")
            } else {
                label
            }
        })
    };
    let row = view().child(text(line).text_color(move || {
        token_ink(style_token(if selected() { Style::Bold } else { Style::Plain }))
    }));
    if WEB_CONTROLS {
        row.role(AccessibilityRole::Button).on_press(move || {
            if let Some(model) = nav.with(|n| n.models.get(i).map(|r| r.model.clone())) {
                let _ = actions.send(UiAction::Nav(NavStep::PickModel(model)));
            }
        })
    } else {
        row
    }
}

/// The help screen: the keys, then the slash commands. Both come from the
/// app's one key table and the shared slash table (see `help::help_lines`).
fn help_body() -> impl IntoElement {
    let mut column = view().flex_col();
    column = column.child(text(HELP_TITLE).font_weight(FontWeight::BOLD));
    for line in help::help_lines() {
        let heading = line == HELP_KEYS_HEADING || line == HELP_COMMANDS_HEADING;
        let style = if heading { Style::Bold } else { Style::Plain };
        column = column
            .child(text(move || line.clone()).text_color(move || token_ink(style_token(style))));
    }
    column
}

/// The sub-agent panel, drawn above the chat composer while the agent
/// screen is open: the row's text, its state, the kill control (web), and
/// the inline confirmation. The composer below it steers (Enter sends a
/// `steer_agent`). After the agent ends the row stays and kill is disabled.
pub(crate) fn agent_panel(
    nav: Signal<NavView>,
    actions: UnboundedSender<UiAction>,
) -> impl IntoElement {
    let title = move || {
        nav.with(|n| {
            n.agent.as_ref().map(|a| a.text.clone()).unwrap_or_else(|| AGENT_NONE_LABEL.to_string())
        })
    };
    let state = move || {
        nav.with(|n| match &n.agent {
            Some(a) if a.running => a.state_word.clone(),
            Some(a) => format!("{} ({AGENT_ENDED_LABEL})", a.state_word),
            None => String::new(),
        })
    };
    let kill_actions = actions.clone();
    let kill = when(
        move || WEB_CONTROLS && nav.with(|n| n.agent_live),
        move || tap(AGENT_KILL_LABEL, kill_actions.clone(), UiAction::Nav(NavStep::AskKill)),
    );
    view()
        .flex_col()
        .child(text(title).text_color(move || token_ink(style_token(Style::Bold))))
        .child(text(state).text_color(muted()))
        .child(kill)
        .child(confirm_line(nav, actions))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_terminal_hint_names_every_opener() {
        for word in ["sessions", "settings", "help"] {
            assert!(TERMINAL_NAV_HINT.contains(word));
        }
    }
}
