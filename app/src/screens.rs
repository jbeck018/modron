//! The screens around the chat body, target-neutral. [`frame`] draws the
//! status surface (connection, model, tokens, mode: on every screen), the
//! navigation controls, and the open screen. Keys are routed by
//! `chat::route_key`; taps send [`UiAction`]s. The words come from the core
//! or the shared table.

use modron_mobile_core::view::{
    AGENT_ENDED_LABEL, AGENT_KILL_LABEL, AGENT_NONE_LABEL, APPROVER_LABEL, AUTH_CODE_LABEL,
    AUTH_KEY_HINT, AUTH_KEY_PLACEHOLDER, AUTH_KEY_SAVE_HINT, AUTH_NO_PROVIDERS_LABEL,
    AUTH_TITLE_LABEL, AUTH_VISIT_LABEL, AUTH_WAITING_LABEL, CONFIRM_NO_LABEL, CONFIRM_YES_LABEL,
    COPIED_LABEL, COPY_LABEL, DELETE_LABEL, HELP_TITLE, MODELS_EMPTY_LABEL, NO_INVITES_LABEL,
    REFRESH_LABEL, REVOKE_LABEL, SHARE_LABEL, SIGN_IN_LABEL, SIGN_IN_PLACEHOLDER, SIGN_OUT_LABEL,
    VIEWER_LABEL,
};
use modron_mobile_core::{
    FILTER_LABEL, NEW_SESSION_LABEL, SESSIONS_EMPTY_LABEL, SESSIONS_LABEL, SESSIONS_NO_MATCH_LABEL,
    SETTINGS_LABEL,
};
use nsh_core::element::{input, AdaptiveColor, AnyElement};
use nsh_core::prelude::*;
use tokio::sync::mpsc::UnboundedSender;

use crate::chat::{self, style_token, token_ink, Model, SecretKey, Style, UiAction, WEB_CONTROLS};
use crate::ui::{self, tok, Bar, Tone};
use crate::ui_chat;
use crate::ui_screens;
use modron_mobile_core::view::{
    AUTOMATIONS_EMPTY_LABEL, AUTOMATIONS_NO_MATCH_LABEL, AUTOMATIONS_TITLE_LABEL,
    AUTOMATION_ADD_LABEL, AUTOMATION_FORM_HINT, SAVE_DEFAULT_HINT, SAVE_DEFAULT_LABEL,
    SIGN_IN_HINT, TASKS_EMPTY_LABEL, TASKS_NO_MATCH_LABEL, TASKS_TITLE_LABEL, TASK_FORM_HINT,
    TASK_MERGE_LABEL, TASK_NEW_LABEL,
};

use crate::help::{HELP_TABS, HELP_VISIBLE};
use crate::nav::{FormKind, NavStep, NavView, PickerRow, Screen};
use modron_mobile_core::view::REMOVE_LABEL;
use modron_mobile_core::view::{
    CLOUD_ACCOUNTS_EMPTY_LABEL, CLOUD_ACCOUNTS_LABEL, CLOUD_ACCOUNT_ADD_LABEL,
    CLOUD_ACCOUNT_PICK_LABEL, CLOUD_BRANCH_LABEL, CLOUD_LABEL, CLOUD_NO_ACCOUNT_LABEL,
    CLOUD_OPEN_LABEL, CLOUD_REPOS_EMPTY_LABEL, CLOUD_REPO_PICK_LABEL, CLOUD_RUNS_EMPTY_LABEL,
    CLOUD_RUNS_LABEL, CLOUD_RUN_LABEL, CLOUD_START_LABEL, CLOUD_STOP_LABEL, CLOUD_TOKEN_HINT,
    CLOUD_TOKEN_PLACEHOLDER, CLOUD_WAKE_LABEL, VAULT_NOTE_LABEL,
};

/// Picker rows the list shows at once (a longer list is filtered, not scrolled).
pub(crate) const PICKER_SLOTS: usize = 16;
/// The mark on the current picker value (a dot, as the old terminal drew it).
pub(crate) const PICKER_CURRENT: &str = "\u{25cf} ";

/// Visible slots: fixed counts keep NSH's cell layout static (see the chat
/// body's notes). Rows past the slot count are not shown.
pub(crate) const SESSION_SLOTS: usize = 12;
pub(crate) const SETTING_SLOTS: usize = 24;
pub(crate) const AUTH_SLOTS: usize = 12;
pub(crate) const MODEL_SLOTS: usize = 16;
pub(crate) const PORTAL_SLOTS: usize = 24;
pub(crate) const INVITE_SLOTS: usize = 8;
pub(crate) const AUTOMATION_SLOTS: usize = 12;
pub(crate) const TASK_SLOTS: usize = 12;
pub(crate) const FORM_SLOTS: usize = 3;
/// Cloud slots: accounts, runs, and one page of repos (the picker's page).
pub(crate) const CLOUD_ACCOUNT_SLOTS: usize = 8;
pub(crate) const CLOUD_RUN_SLOTS: usize = 12;
pub(crate) const CLOUD_REPO_SLOTS: usize = 50;

/// Web controls for the list screens and their forms. App copy: the
/// terminal does not draw these (its keys are the hints above each list).
pub(crate) const FORM_SAVE_LABEL: &str = "save";
pub(crate) const FORM_CANCEL_LABEL: &str = "cancel";
/// The model picker's header word (web). The screen had none; its header now
/// names it.
const MODELS_TITLE: &str = "models";

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
    // The root paints the theme's background token, so a choice made in the
    // picker (or the system appearance) reaches the whole page.
    view()
        .flex_col()
        .size_full()
        .bg(move || token_ink("background"))
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
    // The web's chat draws its own bar (sessions, title and status, settings,
    // help); the other screens show one status caption above their own
    // scaffold. The terminal keeps its header and openers.
    let details = signal(false);
    let frame = view()
        .flex_col()
        .size_full()
        .when(WEB_CONTROLS, |v| v.pt_safe_area().pl_safe_area().pr_safe_area().pb_safe_area());
    let frame = if WEB_CONTROLS {
        frame
            .child(when(move || nav.with(|n| matches!(n.screen, Screen::Chat | Screen::Agent)), {
                let actions = actions.clone();
                move || ui_chat::bar(model, actions.clone(), details)
            }))
            .child(when(move || nav.with(|n| !matches!(n.screen, Screen::Chat | Screen::Agent)), {
                let actions = actions.clone();
                // One status caption; the screen's own bar has "back", so
                // the openers are not repeated here.
                let _ = &actions;
                move || ui_chat::screen_status(model)
            }))
    } else {
        frame
            .child(web_gutter(chat::header(model)))
            .child(web_gutter(
                text(move || model.with(|m| m.status_line.clone())).text_color(muted()),
            ))
            .child(nav_bar(actions.clone()))
    };
    frame
        .child(when(move || nav.with(|n| matches!(n.screen, Screen::Chat | Screen::Agent)), {
            let actions = actions.clone();
            move || chat::screen(model, draft, nav, window_h, details, actions.clone())
        }))
        .child(when(move || nav.with(|n| n.screen == Screen::Sessions), {
            let actions = actions.clone();
            move || {
                ui::screen(
                    scaffold(nav, actions.clone(), SESSIONS_LABEL),
                    if WEB_CONTROLS {
                        ui_screens::sessions(nav, actions.clone()).into_element()
                    } else {
                        sessions_body(nav, actions.clone()).into_element()
                    },
                )
            }
        }))
        .child(when(move || nav.with(|n| n.screen == Screen::Settings), {
            let actions = actions.clone();
            move || {
                ui::screen(
                    scaffold(nav, actions.clone(), SETTINGS_LABEL).form(),
                    if WEB_CONTROLS {
                        ui_screens::settings(nav, actions.clone()).into_element()
                    } else {
                        settings_body(nav, actions.clone()).into_element()
                    },
                )
            }
        }))
        .child(when(move || nav.with(|n| n.screen == Screen::Models), {
            let actions = actions.clone();
            move || {
                ui::screen(
                    scaffold(nav, actions.clone(), MODELS_TITLE).form(),
                    if WEB_CONTROLS {
                        ui_screens::models(nav, actions.clone()).into_element()
                    } else {
                        models_body(nav, actions.clone()).into_element()
                    },
                )
            }
        }))
        .child(when(move || nav.with(|n| n.screen == Screen::Auth), {
            let actions = actions.clone();
            move || {
                ui::screen(
                    scaffold(nav, actions.clone(), AUTH_TITLE_LABEL).form(),
                    if WEB_CONTROLS {
                        ui_screens::credentials(nav, actions.clone()).into_element()
                    } else {
                        auth_body(nav, actions.clone()).into_element()
                    },
                )
            }
        }))
        .child(when(move || nav.with(|n| n.screen == Screen::Automations), {
            let actions = actions.clone();
            move || {
                ui::screen(
                    scaffold(nav, actions.clone(), AUTOMATIONS_TITLE_LABEL),
                    if WEB_CONTROLS {
                        ui_screens::automations(nav, actions.clone()).into_element()
                    } else {
                        automations_body(nav, actions.clone()).into_element()
                    },
                )
            }
        }))
        .child(when(move || nav.with(|n| n.screen == Screen::Tasks), {
            let actions = actions.clone();
            move || {
                ui::screen(
                    scaffold(nav, actions.clone(), TASKS_TITLE_LABEL),
                    if WEB_CONTROLS {
                        ui_screens::tasks(nav, actions.clone()).into_element()
                    } else {
                        tasks_body(nav, actions.clone()).into_element()
                    },
                )
            }
        }))
        .child(when(move || nav.with(|n| n.screen == Screen::Cloud), {
            let actions = actions.clone();
            move || {
                ui::screen(
                    scaffold(nav, actions.clone(), CLOUD_LABEL).form(),
                    if WEB_CONTROLS {
                        ui_screens::cloud(nav, actions.clone()).into_element()
                    } else {
                        cloud_body(nav, actions.clone()).into_element()
                    },
                )
            }
        }))
        .child(when(move || nav.with(|n| n.screen == Screen::CloudRepos), {
            let actions = actions.clone();
            move || {
                ui::screen(
                    scaffold(nav, actions.clone(), CLOUD_REPO_PICK_LABEL).form(),
                    if WEB_CONTROLS {
                        ui_screens::cloud_repos(nav, actions.clone()).into_element()
                    } else {
                        cloud_repos_body(nav, actions.clone()).into_element()
                    },
                )
            }
        }))
        .child(when(move || nav.with(|n| n.screen == Screen::Help), {
            let actions = actions.clone();
            move || {
                ui::screen(
                    scaffold(nav, actions.clone(), HELP_TITLE).form(),
                    if WEB_CONTROLS {
                        ui_screens::help(nav, actions.clone()).into_element()
                    } else {
                        help_body(nav, actions.clone()).into_element()
                    },
                )
            }
        }))
        .child(when(move || nav.with(|n| n.screen == Screen::Picker), {
            let actions = actions.clone();
            move || {
                ui::screen(
                    scaffold(nav, actions.clone(), move || nav.with(|n| n.picker_title.clone()))
                        .form(),
                    if WEB_CONTROLS {
                        ui_screens::picker(nav, actions.clone()).into_element()
                    } else {
                        picker_body(nav, actions.clone()).into_element()
                    },
                )
            }
        }))
}

/// The automations screen (`/automations`): the filter, the rows (the core's
/// text, from `view::automation_row`), the add control, the inline form, and
/// the remove confirmation. The form's fields are text on both targets; the
/// keys are `chat::route_key`'s.
fn automations_body(nav: Signal<NavView>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    let mut column = view().flex_col();
    column = column.child(text(move || nav.with(|n| filter_line(&n.filter))).text_color(muted()));
    column = column.child(text(AUTOMATIONS_TITLE_LABEL).text_color(muted()));
    column = column.child(when(
        move || nav.with(|n| n.automations_total == 0),
        move || text(AUTOMATIONS_EMPTY_LABEL).text_color(muted()),
    ));
    column = column.child(when(
        move || nav.with(|n| n.automations_total > 0 && n.automations.is_empty()),
        move || text(AUTOMATIONS_NO_MATCH_LABEL).text_color(muted()),
    ));
    for i in 0..AUTOMATION_SLOTS {
        let actions = actions.clone();
        column = column.child(when(
            move || nav.with(|n| n.automations.len() > i),
            move || automation_row(nav, i, actions.clone()),
        ));
    }
    column
        .child(when(move || WEB_CONTROLS && nav.with(|n| n.form.is_none()), {
            let actions = actions.clone();
            move || {
                tap(
                    AUTOMATION_ADD_LABEL,
                    actions.clone(),
                    UiAction::Nav(NavStep::OpenForm(FormKind::Automation)),
                )
            }
        }))
        .child(form_body(nav, actions.clone()))
        .child(confirm_line(nav, actions))
}

/// The tasks screen (`/tasks`, `/worktree`): the filter, the rows (title,
/// branch, state), the create control, the inline form, the merge
/// confirmation, and the last note (a task's status or the merge result).
fn tasks_body(nav: Signal<NavView>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    let mut column = view().flex_col();
    column = column.child(text(move || nav.with(|n| filter_line(&n.filter))).text_color(muted()));
    column = column.child(text(TASKS_TITLE_LABEL).text_color(muted()));
    column = column.child(when(
        move || nav.with(|n| n.tasks_total == 0),
        move || text(TASKS_EMPTY_LABEL).text_color(muted()),
    ));
    column = column.child(when(
        move || nav.with(|n| n.tasks_total > 0 && n.tasks.is_empty()),
        move || text(TASKS_NO_MATCH_LABEL).text_color(muted()),
    ));
    for i in 0..TASK_SLOTS {
        let actions = actions.clone();
        column = column.child(when(
            move || nav.with(|n| n.tasks.len() > i),
            move || task_row(nav, i, actions.clone()),
        ));
    }
    column = column.child(when(
        move || nav.with(|n| n.note.is_some()),
        move || text(move || nav.with(|n| n.note.clone().unwrap_or_default())).text_color(muted()),
    ));
    column
        .child(when(move || WEB_CONTROLS && nav.with(|n| n.form.is_none()), {
            let actions = actions.clone();
            move || {
                tap(
                    TASK_NEW_LABEL,
                    actions.clone(),
                    UiAction::Nav(NavStep::OpenForm(FormKind::Task)),
                )
            }
        }))
        .child(form_body(nav, actions.clone()))
        .child(confirm_line(nav, actions))
}

/// One automation row: the shared text, marked when selected. On the web its
/// remove control asks first.
fn automation_row(
    nav: Signal<NavView>,
    i: usize,
    actions: UnboundedSender<UiAction>,
) -> impl IntoElement {
    let selected = move || nav.with(|n| n.selected == i);
    let line = move || {
        nav.with(|n| {
            let row = n.automations.get(i).map(|r| r.text.clone()).unwrap_or_default();
            if n.selected == i {
                format!("> {row}")
            } else {
                row
            }
        })
    };
    let row = view().child(text(line).text_color(move || {
        token_ink(style_token(if selected() { Style::Bold } else { Style::Plain }))
    }));
    if WEB_CONTROLS {
        view().flex_row().child(row).child(tap(
            DELETE_LABEL,
            actions,
            UiAction::Nav(NavStep::AskRemove(i)),
        ))
    } else {
        row
    }
}

/// One task row: the shared text, marked when selected. On the web a tap
/// attaches the task; its merge control asks first.
fn task_row(
    nav: Signal<NavView>,
    i: usize,
    actions: UnboundedSender<UiAction>,
) -> impl IntoElement {
    let selected = move || nav.with(|n| n.selected == i);
    let line = move || {
        nav.with(|n| {
            let row = n.tasks.get(i).map(|r| r.text.clone()).unwrap_or_default();
            if n.selected == i {
                format!("> {row}")
            } else {
                row
            }
        })
    };
    // The row text, then the core's summary under it when one has arrived.
    let row = view()
        .flex_col()
        .child(text(line).text_color(move || {
            token_ink(style_token(if selected() { Style::Bold } else { Style::Plain }))
        }))
        .child(when(
            move || nav.with(|n| n.tasks.get(i).is_some_and(|r| !r.summary.is_empty())),
            move || {
                text(move || {
                    nav.with(|n| {
                        n.tasks.get(i).map(|r| format!("  {}", r.summary)).unwrap_or_default()
                    })
                })
                .text_color(muted())
            },
        ));
    if WEB_CONTROLS {
        let attach = actions.clone();
        let attach_row = row.role(AccessibilityRole::Button).on_press(move || {
            if let Some(id) = nav.with(|n| n.tasks.get(i).map(|r| r.id)) {
                let _ = attach.send(UiAction::Nav(NavStep::Attach(id)));
            }
        });
        view().flex_row().child(attach_row).child(tap(
            TASK_MERGE_LABEL,
            actions,
            UiAction::Nav(NavStep::AskMerge(i)),
        ))
    } else {
        row
    }
}

/// The open inline form: one line per field (the active one marked), its
/// refusal, the form's hint, and on the web the save and cancel controls.
fn form_body(nav: Signal<NavView>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    when(
        move || nav.with(|n| n.form.is_some()),
        move || {
            let mut column = view().flex_col();
            for i in 0..FORM_SLOTS {
                column = column.child(when(
                    move || nav.with(|n| n.form.as_ref().is_some_and(|f| f.labels.len() > i)),
                    move || form_field(nav, i),
                ));
            }
            column = column.child(when(
                move || nav.with(|n| n.form.as_ref().and_then(|f| f.error.clone()).is_some()),
                move || {
                    text(move || {
                        nav.with(|n| {
                            n.form.as_ref().and_then(|f| f.error.clone()).unwrap_or_default()
                        })
                    })
                    .text_color(move || token_ink("warning"))
                },
            ));
            column = column.child(
                text(move || {
                    nav.with(|n| match n.form.as_ref().map(|f| f.kind) {
                        Some(FormKind::Automation) => AUTOMATION_FORM_HINT,
                        _ => TASK_FORM_HINT,
                    })
                    .to_string()
                })
                .text_color(muted()),
            );
            let save = actions.clone();
            let cancel = actions.clone();
            column.child(when(
                move || WEB_CONTROLS,
                move || {
                    view()
                        .flex_row()
                        .child(tap(
                            FORM_SAVE_LABEL,
                            save.clone(),
                            UiAction::Nav(NavStep::FormSubmit),
                        ))
                        .child(tap(
                            FORM_CANCEL_LABEL,
                            cancel.clone(),
                            UiAction::Nav(NavStep::FormCancel),
                        ))
                },
            ))
        },
    )
}

/// One form field: `> label: value` on the active field.
fn form_field(nav: Signal<NavView>, i: usize) -> impl IntoElement {
    text(move || {
        nav.with(|n| {
            n.form
                .as_ref()
                .and_then(|f| {
                    let label = f.labels.get(i)?;
                    let value = f.values.get(i)?;
                    let mark = if f.active == i { ">" } else { " " };
                    Some(format!("{mark} {label}: {value}"))
                })
                .unwrap_or_default()
        })
    })
    .text_color(move || token_ink(style_token(Style::Plain)))
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
    // The vault's note (the keys above are stored for the cloud hosts).
    column = column.child(when(
        move || nav.with(|n| n.vault),
        move || text(VAULT_NOTE_LABEL).text_color(muted()),
    ));
    column = column.child(when(
        move || nav.with(|n| n.vault_note.is_some()),
        move || {
            text(move || nav.with(|n| n.vault_note.clone().unwrap_or_default())).text_color(muted())
        },
    ));
    let confirm_actions = actions.clone();
    column
        .child(when(
            move || nav.with(|n| n.auth_key.is_some()),
            move || auth_key_entry(nav, actions.clone()),
        ))
        .child(confirm_line(nav, confirm_actions))
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
        // A signed-in row also carries the remove control (it asks y / n first).
        let remove_actions = actions.clone();
        let press = row.role(AccessibilityRole::Button).on_press(move || {
            let _ = actions.send(UiAction::Nav(NavStep::ActivateRow(i)));
        });
        view().flex_row().child(press).child(when(
            move || nav.with(|n| n.auth.get(i).is_some_and(|r| r.signed_in())),
            move || {
                tap(REMOVE_LABEL, remove_actions.clone(), UiAction::Nav(NavStep::AskAuthRemove(i)))
            },
        ))
    } else {
        row
    }
}

/// A tap-able text control: a bold line that sends `press` (web only).
fn cloud_button(
    label: impl Fn() -> String + 'static,
    press: impl Fn() + 'static,
) -> impl IntoElement {
    view()
        .role(AccessibilityRole::Button)
        .px(px(8.0))
        .on_press(press)
        .child(text(label).text_color(move || token_ink(style_token(Style::Bold))))
}

/// The Cloud screen (web, owner; PLAN 10.10): the linked GitHub accounts
/// (add by token, remove with a y / n), the new-run form (account, repo,
/// branch, start) and the runs list (open, stop, wake). Every control is a
/// tap; the note line carries the shared words for a refusal or a failure.
fn cloud_body(nav: Signal<NavView>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    let mut column = view().flex_col();
    column = column.child(when(
        move || nav.with(|n| n.cloud.note.is_some()),
        move || {
            text(move || nav.with(|n| n.cloud.note.clone().unwrap_or_default())).text_color(muted())
        },
    ));
    column = column.child(text(CLOUD_ACCOUNTS_LABEL).text_color(muted()));
    column = column.child(when(
        move || nav.with(|n| n.cloud.accounts.is_empty()),
        move || text(CLOUD_ACCOUNTS_EMPTY_LABEL).text_color(muted()),
    ));
    for i in 0..CLOUD_ACCOUNT_SLOTS {
        let actions = actions.clone();
        column = column.child(when(
            move || nav.with(|n| n.cloud.accounts.len() > i),
            move || cloud_account_row(nav, i, actions.clone()),
        ));
    }
    column = column.child(when(move || nav.with(|n| !n.cloud.adding), {
        let actions = actions.clone();
        move || {
            let actions = actions.clone();
            cloud_button(
                || CLOUD_ACCOUNT_ADD_LABEL.to_string(),
                move || {
                    let _ = actions.send(UiAction::Nav(NavStep::CloudAddOpen));
                },
            )
        }
    }));
    column = column.child(when(move || nav.with(|n| n.cloud.adding), {
        let actions = actions.clone();
        move || cloud_token_entry(actions.clone())
    }));
    column = column.child(text(CLOUD_RUN_LABEL).text_color(muted()));
    column = column.child(when(
        move || nav.with(|n| n.cloud.accounts.is_empty()),
        move || text(CLOUD_NO_ACCOUNT_LABEL).text_color(muted()),
    ));
    column = column.child(when(move || nav.with(|n| n.cloud.accounts.len() > 1), {
        let actions = actions.clone();
        move || {
            let actions = actions.clone();
            cloud_button(
                move || {
                    nav.with(|n| {
                        let login =
                            n.cloud.chosen_account().map(|a| a.login.clone()).unwrap_or_default();
                        format!("{CLOUD_ACCOUNT_PICK_LABEL}: {login}")
                    })
                },
                move || {
                    let _ = actions.send(UiAction::Nav(NavStep::CloudPickAccount));
                },
            )
        }
    }));
    column = column.child(when(move || nav.with(|n| !n.cloud.accounts.is_empty()), {
        let actions = actions.clone();
        move || {
            let actions = actions.clone();
            cloud_button(
                move || {
                    nav.with(|n| match &n.cloud.repo {
                        Some(repo) => format!("{CLOUD_REPO_PICK_LABEL}: {}", repo.full_name),
                        None => CLOUD_REPO_PICK_LABEL.to_string(),
                    })
                },
                move || {
                    let _ = actions.send(UiAction::Nav(NavStep::Open(Screen::CloudRepos)));
                },
            )
        }
    }));
    column = column.child(when(move || nav.with(|n| n.cloud.repo.is_some()), {
        let actions = actions.clone();
        move || {
            let actions = actions.clone();
            input()
                .placeholder(CLOUD_BRANCH_LABEL)
                .value(move || nav.with(|n| n.cloud.branch.clone()))
                .on_change(move |s: &str| {
                    let _ = actions.send(UiAction::Nav(NavStep::SetBranch(s.to_string())));
                })
        }
    }));
    column = column.child(when(move || nav.with(|n| n.cloud_start_action().is_some()), {
        let actions = actions.clone();
        move || {
            let actions = actions.clone();
            cloud_button(
                || CLOUD_START_LABEL.to_string(),
                move || {
                    if let Some(action) = nav.with(|n| n.cloud_start_action()) {
                        let _ = actions.send(action);
                    }
                },
            )
        }
    }));
    column = column.child(text(CLOUD_RUNS_LABEL).text_color(muted()));
    column = column.child(when(
        move || nav.with(|n| n.cloud.runs.is_empty()),
        move || text(CLOUD_RUNS_EMPTY_LABEL).text_color(muted()),
    ));
    for i in 0..CLOUD_RUN_SLOTS {
        let actions = actions.clone();
        column = column.child(when(
            move || nav.with(|n| n.cloud.runs.len() > i),
            move || cloud_run_row(nav, i, actions.clone()),
        ));
    }
    column.child(confirm_line(nav, actions))
}

/// One linked account: its line, and a remove control (it asks y / n).
fn cloud_account_row(
    nav: Signal<NavView>,
    i: usize,
    actions: UnboundedSender<UiAction>,
) -> impl IntoElement {
    let line =
        move || nav.with(|n| n.cloud.accounts.get(i).map(|a| a.text.clone()).unwrap_or_default());
    view()
        .flex_row()
        .child(text(line).text_color(move || token_ink(style_token(Style::Plain))))
        .child(tap(REMOVE_LABEL, actions, UiAction::Nav(NavStep::AskAccountRemove(i))))
}

/// One run: its line, then open (when it has a session), stop and wake as
/// its state offers them.
fn cloud_run_row(
    nav: Signal<NavView>,
    i: usize,
    actions: UnboundedSender<UiAction>,
) -> impl IntoElement {
    let line =
        move || nav.with(|n| n.cloud.runs.get(i).map(|r| r.text.clone()).unwrap_or_default());
    let open_actions = actions.clone();
    let stop_actions = actions.clone();
    let wake_actions = actions;
    view()
        .flex_col()
        .child(text(line).text_color(move || token_ink(style_token(Style::Plain))))
        .child(
            view()
                .flex_row()
                .child(when(
                    move || {
                        nav.with(|n| n.cloud.runs.get(i).is_some_and(|r| !r.session_url.is_empty()))
                    },
                    move || {
                        let actions = open_actions.clone();
                        cloud_button(
                            || CLOUD_OPEN_LABEL.to_string(),
                            move || {
                                let _ = actions.send(UiAction::Nav(NavStep::CloudOpenRun(i)));
                            },
                        )
                    },
                ))
                .child(when(
                    move || nav.with(|n| n.cloud.runs.get(i).is_some_and(|r| r.stop)),
                    move || {
                        let actions = stop_actions.clone();
                        cloud_button(
                            || CLOUD_STOP_LABEL.to_string(),
                            move || {
                                if let Some(id) =
                                    nav.with(|n| n.cloud.runs.get(i).map(|r| r.id.clone()))
                                {
                                    let _ = actions.send(UiAction::StopRun(id));
                                }
                            },
                        )
                    },
                ))
                .child(when(
                    move || nav.with(|n| n.cloud.runs.get(i).is_some_and(|r| r.wake)),
                    move || {
                        let actions = wake_actions.clone();
                        cloud_button(
                            || CLOUD_WAKE_LABEL.to_string(),
                            move || {
                                if let Some(id) =
                                    nav.with(|n| n.cloud.runs.get(i).map(|r| r.id.clone()))
                                {
                                    let _ = actions.send(UiAction::WakeRun(id));
                                }
                            },
                        )
                    },
                )),
        )
}

/// The masked token entry for "add account" (the same widget as the key
/// entry). The field is cleared on every submit; Enter and the save control
/// send the same action.
fn cloud_token_entry(actions: UnboundedSender<UiAction>) -> impl IntoElement {
    let token = signal(String::new());
    let submit = actions.clone();
    let press = actions;
    let field = input()
        .masked(true)
        .auto_focus(true)
        .submit_behavior(SubmitBehavior::Submit)
        .placeholder(CLOUD_TOKEN_PLACEHOLDER)
        .value(move || token.get())
        .on_change(move |s: &str| token.set(s.to_string()))
        .on_submit(move |s: &str| {
            send_token(&submit, s);
            token.set(String::new());
        });
    view().flex_col().child(text(CLOUD_TOKEN_HINT).text_color(muted())).child(field).child(
        cloud_button(
            || AUTH_KEY_SAVE_HINT.to_string(),
            move || {
                let value = token.get();
                token.set(String::new());
                send_token(&press, &value);
            },
        ),
    )
}

/// Sends the token for a new linked account and closes the entry. An empty
/// token sends nothing and keeps the entry open.
pub(crate) fn send_token(actions: &UnboundedSender<UiAction>, value: &str) {
    let value = value.trim().to_string();
    if value.is_empty() {
        return;
    }
    let _ = actions
        .send(UiAction::AddAccount(SecretKey { provider: "github".to_string(), key: value }));
    let _ = actions.send(UiAction::Nav(NavStep::CloudAddDone));
}

/// The repo picker (web, owner): the filter line (sent to the server as
/// `q`), one row per repo, and the list's end. A tap on a row uses that repo.
fn cloud_repos_body(nav: Signal<NavView>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    let mut column = view().flex_col();
    column = column.child(text(move || nav.with(|n| filter_line(&n.filter))).text_color(muted()));
    column = column.child(text(CLOUD_REPO_PICK_LABEL).text_color(muted()));
    column = column.child(when(
        move || nav.with(|n| n.cloud.repos.is_empty() && n.cloud.next_page.is_none()),
        move || text(CLOUD_REPOS_EMPTY_LABEL).text_color(muted()),
    ));
    for i in 0..CLOUD_REPO_SLOTS {
        let actions = actions.clone();
        column = column.child(when(
            move || nav.with(|n| n.cloud.repos.len() > i),
            move || cloud_repo_row(nav, i, actions.clone()),
        ));
    }
    column
}

/// One repo row: a press uses that repo (the picker closes).
fn cloud_repo_row(
    nav: Signal<NavView>,
    i: usize,
    actions: UnboundedSender<UiAction>,
) -> impl IntoElement {
    let line =
        move || nav.with(|n| n.cloud.repos.get(i).map(|r| r.text.clone()).unwrap_or_default());
    view()
        .role(AccessibilityRole::Button)
        .on_press(move || {
            let _ = actions.send(UiAction::Nav(NavStep::PickRepo(i)));
        })
        .child(text(line).text_color(move || token_ink(style_token(Style::Plain))))
}

/// The browser sign-in progress: the stage, the message, the URL to open
/// and the device code, from the snapshot's `oauth` (the core's words).
pub(crate) fn auth_progress(nav: Signal<NavView>) -> impl IntoElement {
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
        .when(!WEB_CONTROLS, |v| v.child(text(AUTH_WAITING_LABEL).text_color(muted())))
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
pub(crate) fn send_key(actions: &UnboundedSender<UiAction>, nav: Signal<NavView>, value: &str) {
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
        ui::action(label, actions.clone(), UiAction::Nav(NavStep::Open(screen)))
    };
    ui::gutter(view().flex_row().items_center().gap(px(4.0)).min_h(px(ui::TOUCH_H)))
        .child(opener(SESSIONS_LABEL, Screen::Sessions))
        .child(opener(SETTINGS_LABEL, Screen::Settings))
        .child(opener(HELP_TITLE, Screen::Help))
}

/// The scaffold for a screen reached from the chat frame: its title and the
/// back control (what Esc does).
fn scaffold(
    nav: Signal<NavView>,
    actions: UnboundedSender<UiAction>,
    title: impl IntoReactive<SharedString>,
) -> Bar {
    Bar::new(title).back(nav, actions)
}

/// The portal's header: refresh, cloud (when the account route answers) and
/// sign out. No back control: the list is the root.
fn portal_bar(nav: Signal<NavView>, actions: UnboundedSender<UiAction>) -> Bar {
    let cloud = {
        let actions = actions.clone();
        when(
            move || nav.with(|n| n.cloud.available),
            move || {
                ui::action(
                    CLOUD_LABEL,
                    actions.clone(),
                    UiAction::Nav(NavStep::Open(Screen::Cloud)),
                )
            },
        )
    };
    Bar::new(SESSIONS_LABEL)
        .action(ui::action(REFRESH_LABEL, actions.clone(), UiAction::RefreshPortal))
        .action(cloud)
        .action(ui::action(SIGN_OUT_LABEL, actions, UiAction::SignOut))
}

/// The web's gutter around a chat-frame row (the terminal keeps its layout).
fn web_gutter<V: Styled>(v: V) -> V {
    if WEB_CONTROLS {
        ui::gutter(v)
    } else {
        v
    }
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
        move || nav.with(|n| n.total == 0 && n.sessions_note.is_some()),
        move || {
            text(move || nav.with(|n| n.sessions_note.clone().unwrap_or_default()))
                .text_color(muted())
        },
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
    let push_actions = actions.clone();
    column
        .child(when(
            move || WEB_CONTROLS && nav.with(|n| n.push.is_some()),
            move || push_row(nav, push_actions.clone()),
        ))
        .child(when(
            move || nav.with(|n| n.share.is_some()),
            move || share_section(nav, actions.clone()),
        ))
}

/// The web push row (web only): `notify this device: on|off|unavailable`. A
/// tap on an off or on row sends the toggle; the iOS hint and the last
/// refusal sit under it, muted.
fn push_row(nav: Signal<NavView>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    let line = move || nav.with(|n| n.push.as_ref().map(|p| p.line.clone()).unwrap_or_default());
    let label = view()
        .role(AccessibilityRole::Button)
        .on_press(move || {
            if nav.with(|n| n.push.as_ref().is_some_and(|p| p.tappable)) {
                let _ = actions.send(UiAction::TogglePush);
            }
        })
        .child(text(line).text_color(move || token_ink(style_token(Style::Plain))));
    let hint =
        move || nav.with(|n| n.push.as_ref().and_then(|p| p.hint.clone()).unwrap_or_default());
    let note =
        move || nav.with(|n| n.push.as_ref().and_then(|p| p.note.clone()).unwrap_or_default());
    view()
        .flex_col()
        .child(label)
        .child(when(
            move || nav.with(|n| n.push.as_ref().is_some_and(|p| p.hint.is_some())),
            move || text(hint).text_color(muted()),
        ))
        .child(when(
            move || nav.with(|n| n.push.as_ref().is_some_and(|p| p.note.is_some())),
            move || text(note).text_color(muted()),
        ))
}

/// One settings row: `label: value`. On the web a tap on the label sends the
/// row's action (a read-only row sends nothing), and a removable row shows a
/// remove control that asks first, as `d` does in the terminal.
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
    let label = view().child(text(line).text_color(move || {
        token_ink(style_token(if selected() { Style::Bold } else { Style::Plain }))
    }));
    let press = actions.clone();
    let label = if WEB_CONTROLS {
        label.role(AccessibilityRole::Button).on_press(move || {
            let action = nav.with(|n| n.settings.get(i).and_then(|r| r.action.clone()));
            if let Some(action) = action {
                let _ = press.send(action);
            }
        })
    } else {
        label
    };
    let remove = when(
        move || WEB_CONTROLS && nav.with(|n| n.settings.get(i).is_some_and(|r| r.remove.is_some())),
        move || tap(REMOVE_LABEL, actions.clone(), UiAction::Nav(NavStep::AskSettingRemove(i))),
    );
    view().flex_row().child(label).child(remove)
}

/// The model picker: the filter line, the rows (`model (role)`), and the
/// empty state. Enter (terminal) or a tap (web) switches the model.
fn models_body(nav: Signal<NavView>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    let mut column = view().flex_col();
    column = column.child(text(move || nav.with(|n| filter_line(&n.filter))).text_color(muted()));
    // The terminal's key is named; the web shows the control instead.
    if !WEB_CONTROLS {
        column = column.child(text(SAVE_DEFAULT_HINT).text_color(muted()));
    }
    if WEB_CONTROLS {
        let save_actions = actions.clone();
        column = column.child(when(
            move || nav.with(|n| n.save_default.is_some()),
            move || {
                let save_actions = save_actions.clone();
                view()
                    .role(AccessibilityRole::Button)
                    .on_press(move || {
                        if let Some(action) = nav.with(|n| n.save_default.clone()) {
                            let _ = save_actions.send(action);
                        }
                    })
                    .child(text(SAVE_DEFAULT_LABEL).text_color(move || token_ink("primary")))
            },
        ));
    }
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

/// The help screen: three tabs (General, Commands, Shortcuts) over the rows
/// of the app's key table and the shared slash table (`help::tab_lines`). The
/// rows scroll: Up and Down, PgUp and PgDn, Home and End, and the wheel on
/// the web. On the web a tap on a tab label opens it.
fn help_body(nav: Signal<NavView>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    let mut column = view().flex_col();
    let mut strip = view().flex_row();
    for tab in HELP_TABS {
        let tab_actions = actions.clone();
        let active = move || nav.with(|n| n.help_tab == tab);
        let label = view().px(px(8.0)).child(text(tab.label()).text_color(move || {
            token_ink(style_token(if active() { Style::Bold } else { Style::Plain }))
        }));
        let cell = if WEB_CONTROLS {
            label.role(AccessibilityRole::Button).on_press(move || {
                let _ = tab_actions.send(UiAction::Nav(NavStep::HelpTo(tab)));
            })
        } else {
            label
        };
        strip = strip.child(cell);
    }
    column = column.child(strip);
    for i in 0..HELP_VISIBLE {
        column = column.child(when(
            move || nav.with(|n| n.help_rows.len() > n.help_scroll + i),
            move || {
                text(move || {
                    nav.with(|n| n.help_rows.get(n.help_scroll + i).cloned().unwrap_or_default())
                })
                .text_color(move || token_ink(style_token(Style::Plain)))
            },
        ));
    }
    column.child(
        text(move || {
            nav.with(|n| {
                let total = n.help_rows.len();
                let first = if total == 0 { 0 } else { n.help_scroll + 1 };
                let last = (n.help_scroll + HELP_VISIBLE).min(total);
                format!("{}: {first}-{last} of {total}", n.help_tab.label().to_lowercase())
            })
        })
        .text_color(muted()),
    )
}

/// The picker: its title, the filter line and the rows that match the filter.
/// The current value carries a dot, an unavailable row is dim. Enter (terminal)
/// or a tap (web) applies the row and closes the picker.
fn picker_body(nav: Signal<NavView>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    let mut column = view().flex_col();
    column = column.child(text(move || nav.with(|n| filter_line(&n.filter))).text_color(muted()));
    column = column.child(when(
        move || nav.with(|n| !n.picker_note.is_empty()),
        move || text(move || nav.with(|n| n.picker_note.clone())).text_color(muted()),
    ));
    for i in 0..PICKER_SLOTS {
        let actions = actions.clone();
        column = column.child(when(
            move || nav.with(|n| n.picker.len() > i),
            move || picker_row(nav, i, actions.clone()),
        ));
    }
    column
}

fn picker_row(
    nav: Signal<NavView>,
    i: usize,
    actions: UnboundedSender<UiAction>,
) -> impl IntoElement {
    let selected = move || nav.with(|n| n.selected == i);
    let line = move || {
        nav.with(|n| {
            let row: Option<&PickerRow> = n.picker.get(i);
            let marker = if n.selected == i { "> " } else { "  " };
            let current = if row.is_some_and(|r| r.current) { PICKER_CURRENT } else { "" };
            format!("{marker}{current}{}", row.map(|r| r.label.as_str()).unwrap_or_default())
        })
    };
    let dim = move || nav.with(|n| n.picker.get(i).is_some_and(|r| !r.enabled));
    let row = view().child(text(line).text_color(move || {
        if dim() {
            muted()()
        } else {
            token_ink(style_token(if selected() { Style::Bold } else { Style::Plain }))
        }
    }));
    if WEB_CONTROLS {
        row.role(AccessibilityRole::Button).on_press(move || {
            let _ = actions.send(UiAction::Nav(NavStep::PickRow(i)));
        })
    } else {
        row
    }
}

/// The sub-agent panel, drawn above the chat composer while the agent
/// screen is open: the row's text, its state, the kill control (web), and
/// the inline confirmation. The composer below it steers (Enter sends a
/// `steer_agent`). After the agent ends the row stays and kill is disabled.
pub(crate) fn agent_panel(nav: Signal<NavView>, actions: UnboundedSender<UiAction>) -> AnyElement {
    if WEB_CONTROLS {
        return ui_screens::agent_panel(nav, actions).into_element();
    }
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
        .into_element()
}

/// The portal root: the sign-in screen or the session list.
fn portal_frame(nav: Signal<NavView>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    view()
        .flex_col()
        .size_full()
        .child(when(move || nav.with(|n| n.screen == Screen::SignIn), {
            let actions = actions.clone();
            move || ui::screen(Bar::blank().form(), sign_in_body(nav, actions.clone()))
        }))
        .child(when(move || nav.with(|n| n.screen == Screen::Portal), {
            let actions = actions.clone();
            move || ui::screen(portal_bar(nav, actions.clone()), portal_body(nav, actions.clone()))
        }))
        .child(when(move || nav.with(|n| n.screen == Screen::Cloud), {
            let actions = actions.clone();
            move || {
                ui::screen(
                    scaffold(nav, actions.clone(), CLOUD_LABEL).form(),
                    if WEB_CONTROLS {
                        ui_screens::cloud(nav, actions.clone()).into_element()
                    } else {
                        cloud_body(nav, actions.clone()).into_element()
                    },
                )
            }
        }))
        .child(when(move || nav.with(|n| n.screen == Screen::CloudRepos), {
            let actions = actions.clone();
            move || {
                ui::screen(
                    scaffold(nav, actions.clone(), CLOUD_REPO_PICK_LABEL).form(),
                    if WEB_CONTROLS {
                        ui_screens::cloud_repos(nav, actions.clone()).into_element()
                    } else {
                        cloud_repos_body(nav, actions.clone()).into_element()
                    },
                )
            }
        }))
}

/// The owner sign-in (web): the wordmark, a card with the title, the one
/// explanatory line, the masked token field, the sign-in button and the last
/// attempt's message. Enter and the button send the same action. The page
/// never shows or keeps the token: the field is cleared on every submit.
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
    let button = ui::button(SIGN_IN_LABEL, ui::Variant::Default, ui::Size::Lg, move || {
        let value = token.get().trim().to_string();
        token.set(String::new());
        if !value.is_empty() {
            let _ = press.send(UiAction::SignIn(value));
        }
    })
    .w_full();
    let form = view()
        .flex_col()
        .gap(px(16.0))
        .child(
            text(SIGN_IN_LABEL)
                .text_size(px(24.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(tok("foreground")),
        )
        .child(text(SIGN_IN_HINT).text_size(px(15.0)).text_color(tok("muted-foreground")))
        .child(ui::text_field("", "", field))
        .child(button)
        .child(when(
            move || nav.with(|n| n.signin_error.is_some()),
            move || {
                ui::alert(Tone::Destructive, move || {
                    nav.with(|n| n.signin_error.clone().unwrap_or_default())
                })
                .w_full()
            },
        ));
    view()
        .flex_col()
        .gap(px(24.0))
        .child(wordmark())
        .child(ui::card(ui::card_content(form)).w_full())
}

/// The Modron wordmark for the sign-in card: a gold dot and the name.
fn wordmark() -> impl IntoElement {
    view()
        .flex_row()
        .items_center()
        .justify_center()
        .gap(px(8.0))
        .child(view().w(px(10.0)).h(px(10.0)).rounded_full().bg(tok("primary")))
        .child(
            text("Modron")
                .text_size(px(20.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(tok("foreground")),
        )
}

/// The portal's session list (web): the filter field, the card of rows (a
/// title, the host, the online or offline word and the time, a status dot),
/// and the empty states. Offline rows are dimmed and do not press.
fn portal_body(nav: Signal<NavView>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    let column = view()
        .flex_col()
        .gap(px(16.0))
        .w_full()
        .child(ui::search_field(move || nav.with(|n| n.filter.clone()), FILTER_LABEL))
        .child(when(
            move || nav.with(|n| n.portal_total == 0),
            move || ui::empty_state(SESSIONS_EMPTY_LABEL, "", None),
        ))
        .child(when(
            move || nav.with(|n| n.portal_total > 0 && n.portal.is_empty()),
            move || ui::empty_state(SESSIONS_NO_MATCH_LABEL, "", None),
        ));
    column.child(when(
        move || nav.with(|n| !n.portal.is_empty()),
        move || {
            let mut rows = view().flex_col();
            for i in 0..PORTAL_SLOTS {
                let actions = actions.clone();
                rows = rows.child(when(
                    move || nav.with(|n| n.portal.len() > i),
                    move || portal_row(nav, i, actions.clone()),
                ));
            }
            ui::card_list(rows)
        },
    ))
}

/// One portal row: its title and subtitle, a status dot, the selected row
/// tinted. A press asks the navigation to open it; an offline row is dimmed,
/// and the navigation ignores its press.
fn portal_row(
    nav: Signal<NavView>,
    i: usize,
    actions: UnboundedSender<UiAction>,
) -> impl IntoElement {
    let selected = move || nav.with(|n| n.selected == i);
    let online = move || nav.with(|n| n.portal.get(i).is_some_and(|r| r.online));
    let title = move || nav.with(|n| n.portal.get(i).map(|r| r.title.clone()).unwrap_or_default());
    let subtitle =
        move || nav.with(|n| n.portal.get(i).map(|r| r.subtitle.clone()).unwrap_or_default());
    let dot = ui::status_dot(move || if online() { Tone::Success } else { Tone::Muted });
    ui::list_row(title, subtitle, Some(dot.into_element()), move || {
        let _ = actions.send(UiAction::Nav(NavStep::OpenPortal(i)));
    })
    // An offline row is dimmed; its press still reaches the navigation,
    // which ignores it (as before), so the row is not `disabled`.
    .style_when(move || !online(), |s| s.opacity(0.5))
    .style_when(selected, |s| s.bg(tok("accent")))
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
    view().flex_col().child(created_url_line(nav)).child(view().flex_row().child(copy).child(when(
        move || nav.with(|n| n.share.as_ref().is_some_and(|s| s.note.is_some())),
        move || text(COPIED_LABEL).text_color(muted()),
    )))
}

/// The new invite's URL, whole and never ellipsized. On the web it is a
/// selectable monospace line in a horizontal scroll: a wrap would split the
/// link into several lines (a copied or read link would then be cut at the
/// first line break; a read by a person or the e2e harness is broken), so the
/// link stays one run of text, and the copy control takes it whole. The
/// terminal keeps its text.
fn created_url_line(nav: Signal<NavView>) -> impl IntoElement {
    let line = text(move || {
        nav.with(|n| n.share.as_ref().and_then(|s| s.created.clone()).unwrap_or_default())
    })
    .text_color(move || token_ink("foreground"));
    if WEB_CONTROLS {
        scroll().horizontal().child(line.font_family(MONO_STACK).text_size(px(14.0))).into_element()
    } else {
        line.into_element()
    }
}

/// The monospace stack for a link or a code line (the theme names no font).
pub(crate) const MONO_STACK: &str = "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace";

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
