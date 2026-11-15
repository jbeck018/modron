//! The screens around the chat body, target-neutral. [`frame`] draws the
//! status surface (connection, model, tokens, mode: on every screen), the
//! navigation controls, and the open screen. Keys are routed by
//! `chat::route_key`; taps send [`UiAction`]s. The words come from the core
//! or the shared table.

use modron_mobile_core::view::{
    AGENT_ENDED_LABEL, AGENT_KILL_LABEL, AGENT_NONE_LABEL, APPROVER_LABEL, AUTH_CODE_LABEL,
    AUTH_KEY_HINT, AUTH_KEY_PLACEHOLDER, AUTH_KEY_SAVE_HINT, AUTH_NO_PROVIDERS_LABEL,
    AUTH_VISIT_LABEL, AUTH_WAITING_LABEL, CONFIRM_NO_LABEL, CONFIRM_YES_LABEL, COPIED_LABEL,
    COPY_LABEL, DELETE_LABEL, HELP_COMMANDS_HEADING, HELP_KEYS_HEADING, HELP_TITLE,
    MODELS_EMPTY_LABEL, NO_INVITES_LABEL, REFRESH_LABEL, REVOKE_LABEL, SHARE_LABEL, SIGN_IN_LABEL,
    SIGN_IN_PLACEHOLDER, SIGN_OUT_LABEL, VIEWER_LABEL,
};
use modron_mobile_core::{
    FILTER_LABEL, NEW_SESSION_LABEL, SESSIONS_EMPTY_LABEL, SESSIONS_LABEL, SESSIONS_NO_MATCH_LABEL,
    SETTINGS_LABEL,
};
use nsh_core::element::{input, AdaptiveColor};
use nsh_core::prelude::*;
use tokio::sync::mpsc::UnboundedSender;

use crate::chat::{self, style_token, token_ink, Model, SecretKey, Style, UiAction, WEB_CONTROLS};
use crate::help;
use crate::nav::{NavStep, NavView, Screen};

/// Visible slots: fixed counts keep NSH's cell layout static (see the chat
/// body's notes). Rows past the slot count are not shown.
const SESSION_SLOTS: usize = 12;
const SETTING_SLOTS: usize = 24;
const AUTH_SLOTS: usize = 12;
const MODEL_SLOTS: usize = 16;
const PORTAL_SLOTS: usize = 24;
const INVITE_SLOTS: usize = 8;

/// Terminal hints for the openers. Key names are presentation (the
/// divergence table); the web shows buttons instead.
const TERMINAL_NAV_HINT: &str = "ctrl+o sessions   f2 settings   f1 help";

fn muted() -> impl Fn() -> AdaptiveColor + Clone + 'static {
    || token_ink("muted-foreground")
}

/// The whole app screen: the owner portal (sign-in or the session list)
/// when it is the root, otherwise the chat frame.
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
        .child(when(move || nav.with(|n| n.is_portal()), {
            let actions = actions.clone();
            move || portal_frame(nav, actions.clone())
        }))
        .child(when(
            move || nav.with(|n| !n.is_portal()),
            move || chat_frame(model, draft, nav, window_h, actions.clone()),
        ))
}

/// The chat frame: the status surface and navigation on top, the open
/// screen below it.
fn chat_frame(
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
        .child(when(move || nav.with(|n| n.screen == Screen::Auth), {
            let actions = actions.clone();
            move || auth_body(nav, actions.clone())
        }))
        .child(when(move || nav.with(|n| n.screen == Screen::Help), help_body))
}

/// The credentials screen (parity D2): the core's provider rows (label and
/// status verbatim), the browser sign-in progress, and the masked key
/// entry for a key-only provider. Enter (terminal) or a tap (web) starts
/// the sign-in or opens the entry.
fn auth_body(nav: Signal<NavView>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    let mut column = view().flex_col();
    column = column.child(when(
        move || nav.with(|n| n.auth.is_empty()),
        move || text(AUTH_NO_PROVIDERS_LABEL).text_color(muted()),
    ));
    for i in 0..AUTH_SLOTS {
        let actions = actions.clone();
        column = column.child(when(
            move || nav.with(|n| n.auth.len() > i),
            move || auth_row(nav, i, actions.clone()),
        ));
    }
    column = column
        .child(when(move || nav.with(|n| n.auth_progress.is_some()), move || auth_progress(nav)));
    column.child(when(
        move || nav.with(|n| n.auth_key.is_some()),
        move || auth_key_entry(nav, actions.clone()),
    ))
}

/// One credentials row: `label: status`, marked when selected. Its hint
/// (sign in or paste a key) shows on the selected row.
fn auth_row(
    nav: Signal<NavView>,
    i: usize,
    actions: UnboundedSender<UiAction>,
) -> impl IntoElement {
    let selected = move || nav.with(|n| n.selected == i);
    let line = move || {
        nav.with(|n| {
            let line = n.auth.get(i).map(|r| r.line()).unwrap_or_default();
            if n.selected == i {
                format!("> {line}")
            } else {
                line
            }
        })
    };
    let row = view().flex_col().child(text(line).text_color(move || {
        token_ink(style_token(if selected() { Style::Bold } else { Style::Plain }))
    }));
    if WEB_CONTROLS {
        row.role(AccessibilityRole::Button).on_press(move || {
            let _ = actions.send(UiAction::Nav(NavStep::ActivateRow(i)));
        })
    } else {
        row
    }
}

/// The browser sign-in progress: the stage, the message, the URL to open
/// and the device code, from the snapshot's `oauth` (the core's words).
fn auth_progress(nav: Signal<NavView>) -> impl IntoElement {
    let head = move || {
        nav.with(|n| {
            n.auth_progress
                .as_ref()
                .map(|p| format!("{}: {}", p.provider, p.stage))
                .unwrap_or_default()
        })
    };
    let message = move || {
        nav.with(|n| n.auth_progress.as_ref().map(|p| p.message.clone()).unwrap_or_default())
    };
    view()
        .flex_col()
        .child(text(head).font_weight(FontWeight::BOLD))
        .child(text(message))
        .child(when(
            move || nav.with(|n| n.auth_progress.as_ref().and_then(|p| p.url.as_ref()).is_some()),
            move || {
                text(move || {
                    nav.with(|n| {
                        let url = n.auth_progress.as_ref().and_then(|p| p.url.clone());
                        format!("{AUTH_VISIT_LABEL}: {}", url.unwrap_or_default())
                    })
                })
            },
        ))
        .child(when(
            move || {
                nav.with(|n| n.auth_progress.as_ref().and_then(|p| p.user_code.as_ref()).is_some())
            },
            move || {
                text(move || {
                    nav.with(|n| {
                        let code = n.auth_progress.as_ref().and_then(|p| p.user_code.clone());
                        format!("{AUTH_CODE_LABEL}: {}", code.unwrap_or_default())
                    })
                })
            },
        ))
        .child(text(AUTH_WAITING_LABEL).text_color(muted()))
}

/// The masked key entry for the open provider (`input().masked(true)`, the
/// same widget as the owner sign-in). The field is a view signal that is
/// cleared on every submit and dropped when the entry closes, so the key
/// is never kept by the app. Enter sends the key, then closes the entry.
fn auth_key_entry(nav: Signal<NavView>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    let key = signal(String::new());
    let submit = actions.clone();
    let press = actions.clone();
    let field = input()
        .masked(true)
        .auto_focus(true)
        .submit_behavior(SubmitBehavior::Submit)
        .placeholder(AUTH_KEY_PLACEHOLDER)
        .value(move || key.get())
        .on_change(move |s: &str| key.set(s.to_string()))
        .on_submit(move |s: &str| {
            send_key(&submit, nav, s);
            key.set(String::new());
        });
    let save = view()
        .role(AccessibilityRole::Button)
        .px(px(8.0))
        .on_press(move || {
            let value = key.get();
            key.set(String::new());
            send_key(&press, nav, &value);
        })
        .child(text(AUTH_KEY_SAVE_HINT).text_color(move || token_ink(style_token(Style::Bold))));
    view()
        .flex_col()
        .child(text(move || {
            nav.with(|n| {
                let label = n
                    .auth_key
                    .as_ref()
                    .and_then(|id| n.auth.iter().find(|r| &r.id == id))
                    .map(|r| r.label.clone())
                    .unwrap_or_default();
                format!("{label}: {AUTH_KEY_HINT}")
            })
        }))
        .child(field)
        .child(save)
}

/// Sends the key for the open provider and closes the entry. An empty key
/// sends nothing and keeps the entry open.
fn send_key(actions: &UnboundedSender<UiAction>, nav: Signal<NavView>, value: &str) {
    let value = value.trim().to_string();
    if value.is_empty() {
        return;
    }
    if let Some(provider) = nav.with(|n| n.auth_key.clone()) {
        let _ = actions.send(UiAction::SetKey(SecretKey { provider, key: value }));
        let _ = actions.send(UiAction::Nav(NavStep::AuthKeyDone));
    }
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
    column.child(when(
        move || nav.with(|n| n.share.is_some()),
        move || share_section(nav, actions.clone()),
    ))
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
    // An unavailable model (its provider is not signed in) is dim.
    let dim = move || nav.with(|n| n.models.get(i).is_some_and(|r| !r.available));
    let row = view().child(text(line).text_color(move || {
        if dim() {
            muted()()
        } else {
            token_ink(style_token(if selected() { Style::Bold } else { Style::Plain }))
        }
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

/// The portal root: the sign-in screen or the session list.
fn portal_frame(nav: Signal<NavView>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    view()
        .flex_col()
        .size_full()
        .child(when(move || nav.with(|n| n.screen == Screen::SignIn), {
            let actions = actions.clone();
            move || sign_in_body(nav, actions.clone())
        }))
        .child(when(
            move || nav.with(|n| n.screen == Screen::Portal),
            move || portal_body(nav, actions.clone()),
        ))
}

/// The owner sign-in: a masked token input (the page never shows or keeps
/// the token: the input is cleared on every submit), a sign-in control and
/// the last attempt's message. Enter and the control send the same action.
fn sign_in_body(nav: Signal<NavView>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    let token = signal(String::new());
    let submit = actions.clone();
    let press = actions.clone();
    let field = input()
        .masked(true)
        .auto_focus(true)
        .submit_behavior(SubmitBehavior::Submit)
        .placeholder(SIGN_IN_PLACEHOLDER)
        .value(move || token.get())
        .on_change(move |s: &str| token.set(s.to_string()))
        .on_submit(move |s: &str| {
            let value = s.trim().to_string();
            token.set(String::new());
            if !value.is_empty() {
                let _ = submit.send(UiAction::SignIn(value));
            }
        });
    let button = view()
        .role(AccessibilityRole::Button)
        .px(px(8.0))
        .on_press(move || {
            let value = token.get().trim().to_string();
            token.set(String::new());
            if !value.is_empty() {
                let _ = press.send(UiAction::SignIn(value));
            }
        })
        .child(text(SIGN_IN_LABEL).text_color(move || token_ink(style_token(Style::Bold))));
    view()
        .flex_col()
        .child(text(SIGN_IN_LABEL).font_weight(FontWeight::BOLD))
        .child(field)
        .child(button)
        .child(when(
            move || nav.with(|n| n.signin_error.is_some()),
            move || {
                text(move || nav.with(|n| n.signin_error.clone().unwrap_or_default()))
                    .text_color(move || token_ink("destructive"))
            },
        ))
}

/// The portal's session list: the header (refresh, sign out), the filter
/// line, the empty states and one line per row. Offline rows are dim; a
/// press opens an online row (the navigation decides).
fn portal_body(nav: Signal<NavView>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    let header = view()
        .flex_row()
        .child(text(SESSIONS_LABEL).font_weight(FontWeight::BOLD))
        .child(tap(REFRESH_LABEL, actions.clone(), UiAction::RefreshPortal))
        .child(tap(SIGN_OUT_LABEL, actions.clone(), UiAction::SignOut));
    let mut column = view().flex_col().child(header);
    column = column.child(text(move || nav.with(|n| filter_line(&n.filter))).text_color(muted()));
    column = column.child(when(
        move || nav.with(|n| n.portal_total == 0),
        move || text(SESSIONS_EMPTY_LABEL).text_color(muted()),
    ));
    column = column.child(when(
        move || nav.with(|n| n.portal_total > 0 && n.portal.is_empty()),
        move || text(SESSIONS_NO_MATCH_LABEL).text_color(muted()),
    ));
    for i in 0..PORTAL_SLOTS {
        let actions = actions.clone();
        column = column.child(when(
            move || nav.with(|n| n.portal.len() > i),
            move || portal_row(nav, i, actions.clone()),
        ));
    }
    column
}

/// One portal row: its line, marked when selected, and a press that asks
/// the navigation to open it.
fn portal_row(
    nav: Signal<NavView>,
    i: usize,
    actions: UnboundedSender<UiAction>,
) -> impl IntoElement {
    let selected = move || nav.with(|n| n.selected == i);
    let online = move || nav.with(|n| n.portal.get(i).is_some_and(|r| r.online));
    let line = move || {
        nav.with(|n| {
            let text = n.portal.get(i).map(|r| r.text.clone()).unwrap_or_default();
            if n.selected == i {
                format!("> {text}")
            } else {
                text
            }
        })
    };
    view()
        .role(AccessibilityRole::Button)
        .on_press(move || {
            let _ = actions.send(UiAction::Nav(NavStep::OpenPortal(i)));
        })
        .child(text(line).text_color(move || {
            if online() {
                token_ink(style_token(if selected() { Style::Bold } else { Style::Plain }))
            } else {
                token_ink(style_token(Style::Meta))
            }
        }))
}

/// The Settings share section (owner, a connected session): create a viewer
/// or approver invite, show the new link with its copy control, list the
/// invites with a revoke control each.
fn share_section(nav: Signal<NavView>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    let roles = view()
        .flex_row()
        .child(tap(VIEWER_LABEL, actions.clone(), UiAction::CreateInvite(VIEWER_LABEL.into())))
        .child(tap(APPROVER_LABEL, actions.clone(), UiAction::CreateInvite(APPROVER_LABEL.into())));
    let mut column =
        view().flex_col().child(text(SHARE_LABEL).font_weight(FontWeight::BOLD)).child(roles);
    column = column.child(when(
        move || nav.with(|n| n.share.as_ref().is_some_and(|s| s.created.is_some())),
        {
            let actions = actions.clone();
            move || created_invite(nav, actions.clone())
        },
    ));
    column = column.child(when(
        move || nav.with(|n| n.share.as_ref().is_some_and(|s| s.invites.is_empty())),
        move || text(NO_INVITES_LABEL).text_color(muted()),
    ));
    for i in 0..INVITE_SLOTS {
        let actions = actions.clone();
        column = column.child(when(
            move || nav.with(|n| n.share.as_ref().is_some_and(|s| s.invites.len() > i)),
            move || invite_line(nav, i, actions.clone()),
        ));
    }
    column
}

/// The new invite's URL as selectable text, with the copy control. The
/// copied word follows a successful copy.
fn created_invite(nav: Signal<NavView>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    let copy = view()
        .role(AccessibilityRole::Button)
        .px(px(8.0))
        .on_press(move || {
            if let Some(url) = nav.with(|n| n.share.as_ref().and_then(|s| s.created.clone())) {
                let _ = actions.send(UiAction::CopyInvite(url));
            }
        })
        .child(text(COPY_LABEL).text_color(move || token_ink(style_token(Style::Plain))));
    view()
        .flex_col()
        .child(
            text(move || {
                nav.with(|n| n.share.as_ref().and_then(|s| s.created.clone()).unwrap_or_default())
            })
            .text_color(move || token_ink("foreground")),
        )
        .child(view().flex_row().child(copy).child(when(
            move || nav.with(|n| n.share.as_ref().is_some_and(|s| s.note.is_some())),
            move || text(COPIED_LABEL).text_color(muted()),
        )))
}

/// One invite: its line and the revoke control (the id is read at press
/// time, so the row always revokes the invite it shows).
fn invite_line(
    nav: Signal<NavView>,
    i: usize,
    actions: UnboundedSender<UiAction>,
) -> impl IntoElement {
    let line = move || {
        nav.with(|n| {
            n.share
                .as_ref()
                .and_then(|s| s.invites.get(i))
                .map(|l| l.text.clone())
                .unwrap_or_default()
        })
    };
    let revoke = view()
        .role(AccessibilityRole::Button)
        .px(px(8.0))
        .on_press(move || {
            if let Some(id) =
                nav.with(|n| n.share.as_ref().and_then(|s| s.invites.get(i)).map(|l| l.id.clone()))
            {
                let _ = actions.send(UiAction::RevokeInvite(id));
            }
        })
        .child(text(REVOKE_LABEL).text_color(move || token_ink(style_token(Style::Plain))));
    view().flex_row().child(text(line).text_color(muted())).child(revoke)
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
