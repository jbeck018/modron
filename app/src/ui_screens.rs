//! The web target's screens (Program UX-3): settings, credentials, the
//! pickers, help, the sessions, tasks and automations lists, and Cloud. Each
//! body is drawn from the `ui` components in the shadcn style (cards, rows,
//! switches, tabs, buttons). The words, rows, states and actions are the
//! shared ones, and `screens.rs` chooses between these bodies and the
//! terminal's on `WEB_CONTROLS`. Nothing here is drawn on the terminal.
//!
//! The rules are in `app/DESIGN.md`. Keyboard-only hints are not drawn here
//! where a visible control does the same thing (the form hints, the key
//! hints on the save controls); their constants stay in the shared table.

use modron_mobile_core::view::{
    AGENT_ENDED_LABEL, AGENT_KILL_LABEL, AGENT_NONE_LABEL, APPROVER_LABEL, AUTH_KEY_HINT,
    AUTH_KEY_PLACEHOLDER, AUTH_NO_PROVIDERS_LABEL, AUTH_ROW_KEY_LABEL, AUTH_ROW_SIGN_IN_LABEL,
    AUTOMATIONS_EMPTY_LABEL, AUTOMATIONS_NO_MATCH_LABEL, AUTOMATION_ADD_LABEL,
    CLOUD_ACCOUNTS_EMPTY_LABEL, CLOUD_ACCOUNTS_LABEL, CLOUD_ACCOUNT_ADD_LABEL,
    CLOUD_ACCOUNT_PICK_LABEL, CLOUD_BRANCH_LABEL, CLOUD_NO_ACCOUNT_LABEL, CLOUD_OPEN_LABEL,
    CLOUD_REPOS_EMPTY_LABEL, CLOUD_REPO_PICK_LABEL, CLOUD_RUNS_EMPTY_LABEL, CLOUD_RUNS_LABEL,
    CLOUD_RUN_LABEL, CLOUD_START_LABEL, CLOUD_STOP_LABEL, CLOUD_TOKEN_HINT,
    CLOUD_TOKEN_PLACEHOLDER, CLOUD_WAKE_LABEL, CONFIRM_NO_LABEL, CONFIRM_YES_LABEL, COPIED_LABEL,
    COPY_LABEL, DELETE_LABEL, MODELS_EMPTY_LABEL, NO_INVITES_LABEL, PUSH_LABEL, REMOVE_LABEL,
    REVOKE_LABEL, SAVE_DEFAULT_LABEL, SHARE_LABEL, TASKS_EMPTY_LABEL, TASKS_NO_MATCH_LABEL,
    TASK_MERGE_LABEL, TASK_NEW_LABEL, VAULT_NOTE_LABEL, VIEWER_LABEL,
};
use modron_mobile_core::{
    FILTER_LABEL, NEW_SESSION_LABEL, SESSIONS_EMPTY_LABEL, SESSIONS_NO_MATCH_LABEL,
};
use nsh_core::element::input;
use nsh_core::prelude::*;
use tokio::sync::mpsc::UnboundedSender;

use crate::chat::UiAction;
use crate::help::{shown_tabs, HELP_VISIBLE};
use crate::nav::{NavStep, NavView, Screen};
use crate::screens::{
    auth_progress, send_key, send_token, AUTH_SLOTS, AUTOMATION_SLOTS, CLOUD_ACCOUNT_SLOTS,
    CLOUD_REPO_SLOTS, CLOUD_RUN_SLOTS, FORM_CANCEL_LABEL, FORM_SAVE_LABEL, FORM_SLOTS,
    INVITE_SLOTS, MODEL_SLOTS, MONO_STACK, PICKER_CURRENT, PICKER_SLOTS, SESSION_SLOTS,
    SETTING_SLOTS, TASK_SLOTS,
};
use crate::ui::{self, tok, Size, Tone, Variant};

/// The Settings screen: the rows as one card (the web push row last), the
/// share section, and the removal confirmation.
pub(crate) fn settings(
    nav: Signal<NavView>,
    actions: UnboundedSender<UiAction>,
) -> impl IntoElement {
    let mut rows = view().flex_col();
    for i in 0..SETTING_SLOTS {
        let actions = actions.clone();
        rows = rows.child(when(
            move || nav.with(|n| n.settings.len() > i),
            move || setting_row(nav, i, actions.clone()),
        ));
    }
    let push_actions = actions.clone();
    let share_actions = actions.clone();
    let push =
        when(move || nav.with(|n| n.push.is_some()), move || push_row(nav, push_actions.clone()));
    view()
        .flex_col()
        .gap(px(24.0))
        .child(ui::card_list(rows.child(push)))
        .child(when(
            move || nav.with(|n| n.share.is_some()),
            move || share_section(nav, share_actions.clone()),
        ))
        .child(confirm(nav, actions))
}

/// One settings row: its label, its value on the right, and a removal control
/// (a destructive button, which asks first) when the row can be removed.
fn setting_row(
    nav: Signal<NavView>,
    i: usize,
    actions: UnboundedSender<UiAction>,
) -> impl IntoElement {
    let label =
        move || nav.with(|n| n.settings.get(i).map(|r| r.label.clone()).unwrap_or_default());
    let value =
        move || nav.with(|n| n.settings.get(i).map(|r| r.value.clone()).unwrap_or_default());
    let press = actions.clone();
    let shown = move || nav.with(|n| n.settings.get(i).is_some_and(|r| r.remove.is_some()));
    let remove = when(shown, move || {
        let remove = actions.clone();
        ui::button(REMOVE_LABEL, Variant::Destructive, Size::Sm, move || {
            let _ = remove.send(UiAction::Nav(NavStep::AskSettingRemove(i)));
        })
    });
    ui::row(ui::value_pair(label, value), Some(ui::trailing(remove, shown)), move || {
        if let Some(action) = nav.with(|n| n.settings.get(i).and_then(|r| r.action.clone())) {
            let _ = press.send(action);
        }
    })
}

/// The web push row: its label and value, a switch that turns it on or off
/// (disabled when the row is not tappable), and the hint and the note under it.
fn push_row(nav: Signal<NavView>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    let label =
        move || nav.with(|n| n.push.as_ref().map(|p| p.label.to_string()).unwrap_or_default());
    let value =
        move || nav.with(|n| n.push.as_ref().map(|p| p.value.to_string()).unwrap_or_default());
    let on = move || nav.with(|n| n.push.as_ref().is_some_and(|p| p.on));
    let tappable = move || nav.with(|n| n.push.as_ref().is_some_and(|p| p.tappable));
    let switch = ui::switch(
        on,
        move || !tappable(),
        PUSH_LABEL,
        move || {
            if tappable() {
                let _ = actions.send(UiAction::TogglePush);
            }
        },
    );
    let hint =
        move || nav.with(|n| n.push.as_ref().and_then(|p| p.hint.clone()).unwrap_or_default());
    let note =
        move || nav.with(|n| n.push.as_ref().and_then(|p| p.note.clone()).unwrap_or_default());
    view()
        .flex_col()
        .child(ui::static_row(ui::value_pair(label, value), Some(ui::trailing(switch, true))))
        .child(when(
            move || nav.with(|n| n.push.as_ref().is_some_and(|p| p.hint.is_some())),
            move || view().px(px(ui::GUTTER)).py(px(8.0)).child(ui::caption(hint)),
        ))
        .child(when(
            move || nav.with(|n| n.push.as_ref().is_some_and(|p| p.note.is_some())),
            move || {
                view().px(px(ui::GUTTER)).py(px(8.0)).child(ui::alert(Tone::Warning, note).w_full())
            },
        ))
}

/// The share section: the two invite roles, the new link with its copy control,
/// and the invites, each with a revoke control.
fn share_section(nav: Signal<NavView>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    let viewer = actions.clone();
    let approver = actions.clone();
    let roles = view()
        .flex_row()
        .gap(px(8.0))
        .child(
            ui::button(VIEWER_LABEL, Variant::Outline, Size::Sm, move || {
                let _ = viewer.send(UiAction::CreateInvite(VIEWER_LABEL.into()));
            })
            .flex_1(),
        )
        .child(
            ui::button(APPROVER_LABEL, Variant::Outline, Size::Sm, move || {
                let _ = approver.send(UiAction::CreateInvite(APPROVER_LABEL.into()));
            })
            .flex_1(),
        );
    let created_actions = actions.clone();
    let created = when(
        move || nav.with(|n| n.share.as_ref().is_some_and(|s| s.created.is_some())),
        move || created_invite(nav, created_actions.clone()),
    );
    let card =
        ui::card(ui::card_content(view().flex_col().gap(px(16.0)).child(roles).child(created)));
    let invites_actions = actions;
    view()
        .flex_col()
        .gap(px(16.0))
        .child(ui::section_title(SHARE_LABEL))
        .child(card)
        .child(when(
            move || nav.with(|n| n.share.as_ref().is_some_and(|s| s.invites.is_empty())),
            move || ui::caption(NO_INVITES_LABEL),
        ))
        .child(when(
            move || nav.with(|n| n.share.as_ref().is_some_and(|s| !s.invites.is_empty())),
            move || {
                let mut rows = view().flex_col();
                for i in 0..INVITE_SLOTS {
                    let actions = invites_actions.clone();
                    rows = rows.child(when(
                        move || nav.with(|n| n.share.as_ref().is_some_and(|s| s.invites.len() > i)),
                        move || invite_row(nav, i, actions.clone()),
                    ));
                }
                ui::card_list(rows)
            },
        ))
}

/// The new invite's link: one line of monospace text (it scrolls sideways, so
/// it is never wrapped), then the copy control and the copied word.
fn created_invite(nav: Signal<NavView>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    let link = text(move || {
        nav.with(|n| n.share.as_ref().and_then(|s| s.created.clone()).unwrap_or_default())
    })
    .font_family(MONO_STACK)
    .text_size(px(14.0))
    .text_color(tok("foreground"));
    let copy = ui::button(COPY_LABEL, Variant::Outline, Size::Sm, move || {
        if let Some(url) = nav.with(|n| n.share.as_ref().and_then(|s| s.created.clone())) {
            let _ = actions.send(UiAction::CopyInvite(url));
        }
    });
    view()
        .flex_col()
        .gap(px(8.0))
        .child(
            view()
                .rounded(px(ui::radius()))
                .bg(tok("muted"))
                .px(px(12.0))
                .py(px(8.0))
                .child(scroll().horizontal().child(link)),
        )
        .child(view().flex_row().items_center().gap(px(8.0)).child(copy).child(when(
            move || nav.with(|n| n.share.as_ref().is_some_and(|s| s.note.is_some())),
            move || ui::caption(COPIED_LABEL),
        )))
}

/// One invite: its line, and the revoke control (the id is read at press time).
fn invite_row(
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
    let revoke = ui::button(REVOKE_LABEL, Variant::Destructive, Size::Sm, move || {
        if let Some(id) =
            nav.with(|n| n.share.as_ref().and_then(|s| s.invites.get(i)).map(|l| l.id.clone()))
        {
            let _ = actions.send(UiAction::RevokeInvite(id));
        }
    });
    ui::static_row(view().flex_1().child(ui::caption(line)), Some(ui::trailing(revoke, true)))
}

/// The credentials screen: the provider rows as one card, the browser sign-in
/// progress, the vault notes, the masked key entry, and the removal confirmation.
pub(crate) fn credentials(
    nav: Signal<NavView>,
    actions: UnboundedSender<UiAction>,
) -> impl IntoElement {
    let rows_actions = actions.clone();
    let key_actions = actions.clone();
    view()
        .flex_col()
        .gap(px(24.0))
        .child(when(
            move || nav.with(|n| n.auth.is_empty()),
            move || ui::caption(AUTH_NO_PROVIDERS_LABEL),
        ))
        .child(when(
            move || nav.with(|n| !n.auth.is_empty()),
            move || {
                let mut rows = view().flex_col();
                for i in 0..AUTH_SLOTS {
                    let actions = rows_actions.clone();
                    rows = rows.child(when(
                        move || nav.with(|n| n.auth.len() > i),
                        move || credential_row(nav, i, actions.clone()),
                    ));
                }
                ui::card_list(rows)
            },
        ))
        .child(when(
            move || nav.with(|n| n.auth_progress.is_some()),
            move || ui::card(ui::card_content(auth_progress(nav))),
        ))
        .child(when(move || nav.with(|n| n.vault), move || ui::caption(VAULT_NOTE_LABEL)))
        .child(when(
            move || nav.with(|n| n.vault_note.is_some()),
            move || ui::caption(move || nav.with(|n| n.vault_note.clone().unwrap_or_default())),
        ))
        .child(when(
            move || nav.with(|n| n.auth_key.is_some()),
            move || key_entry(nav, key_actions.clone()),
        ))
        .child(confirm(nav, actions))
}

/// One provider row: its label and status, and a removal control when it is
/// signed in. A press starts the sign-in or opens the key entry.
fn credential_row(
    nav: Signal<NavView>,
    i: usize,
    actions: UnboundedSender<UiAction>,
) -> impl IntoElement {
    let label = move || nav.with(|n| n.auth.get(i).map(|r| r.label.clone()).unwrap_or_default());
    let status = move || nav.with(|n| n.auth.get(i).map(|r| r.status.clone()).unwrap_or_default());
    let press = actions.clone();
    let sign_in = actions.clone();
    let key = actions.clone();
    let shown = move || nav.with(|n| n.auth.get(i).is_some_and(|r| r.signed_in()));
    let remove = when(shown, move || {
        let remove = actions.clone();
        ui::button(REMOVE_LABEL, Variant::Destructive, Size::Sm, move || {
            let _ = remove.send(UiAction::Nav(NavStep::AskAuthRemove(i)));
        })
    });
    let row =
        ui::row(ui::value_pair(label, status), Some(ui::trailing(remove, shown)), move || {
            let _ = press.send(UiAction::Nav(NavStep::ActivateRow(i)));
        });
    // An OAuth-capable row has both actions as buttons: the browser sign-in
    // and the key entry. A key-only row has its press (the key entry) alone.
    let oauth = move || nav.with(|n| n.auth.get(i).is_some_and(|r| r.oauth_capable));
    let buttons = when(oauth, move || {
        let sign_in = sign_in.clone();
        let key = key.clone();
        view()
            .flex_row()
            .gap(px(8.0))
            .px(px(ui::GUTTER))
            .py(px(8.0))
            .child(ui::button(AUTH_ROW_SIGN_IN_LABEL, Variant::Outline, Size::Sm, move || {
                let _ = sign_in.send(UiAction::Nav(NavStep::SignInRow(i)));
            }))
            .child(ui::button(AUTH_ROW_KEY_LABEL, Variant::Outline, Size::Sm, move || {
                let _ = key.send(UiAction::Nav(NavStep::KeyRow(i)));
            }))
    });
    view().flex_col().child(row).child(buttons)
}

/// The masked key entry for the open provider. The field is a view signal that
/// is cleared on every submit and dropped when the entry closes.
fn key_entry(nav: Signal<NavView>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    let key = signal(String::new());
    let submit = actions.clone();
    let press = actions;
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
    let save = ui::button(FORM_SAVE_LABEL, Variant::Default, Size::Default, move || {
        let value = key.get();
        key.set(String::new());
        send_key(&press, nav, &value);
    })
    .w_full();
    let title = move || {
        nav.with(|n| {
            let label = n
                .auth_key
                .as_ref()
                .and_then(|id| n.auth.iter().find(|r| &r.id == id))
                .map(|r| r.label.clone())
                .unwrap_or_default();
            format!("{label}: {AUTH_KEY_HINT}")
        })
    };
    ui::card(ui::card_content(
        view()
            .flex_col()
            .gap(px(12.0))
            .child(ui::caption(title))
            .child(ui::text_field("", "", field))
            .child(save),
    ))
}

/// The picker screen: the filter, the note, and the rows, the current value
/// marked and an unavailable row dim. A tap applies the row and closes it.
pub(crate) fn picker(nav: Signal<NavView>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    let rows_actions = actions.clone();
    let rows = move || {
        let mut rows = view().flex_col();
        for i in 0..PICKER_SLOTS {
            let actions = rows_actions.clone();
            rows = rows.child(when(
                move || nav.with(|n| n.picker.len() > i),
                move || picker_row(nav, i, actions.clone()),
            ));
        }
        rows
    };
    view()
        .flex_col()
        .gap(px(16.0))
        .child(ui::search_field(move || nav.with(|n| n.filter.clone()), FILTER_LABEL))
        .child(when(
            move || nav.with(|n| !n.picker_note.is_empty()),
            move || ui::caption(move || nav.with(|n| n.picker_note.clone())),
        ))
        .child(when(move || nav.with(|n| !n.picker.is_empty()), move || ui::card_list(rows())))
}

fn picker_row(
    nav: Signal<NavView>,
    i: usize,
    actions: UnboundedSender<UiAction>,
) -> impl IntoElement {
    let label = move || nav.with(|n| n.picker.get(i).map(|r| r.label.clone()).unwrap_or_default());
    ui::choice_row(
        PICKER_CURRENT.trim(),
        move || nav.with(|n| n.picker.get(i).is_some_and(|r| r.current)),
        move || nav.with(|n| n.selected == i),
        move || nav.with(|n| n.picker.get(i).is_some_and(|r| !r.enabled)),
        label,
        move || {
            let _ = actions.send(UiAction::Nav(NavStep::PickRow(i)));
        },
    )
}

/// The model picker: the filter, the save-as-default control, and the rows
/// (`model (role)`), the current model marked and an unavailable one dim.
pub(crate) fn models(nav: Signal<NavView>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    let save_actions = actions.clone();
    let rows_actions = actions.clone();
    let rows = move || {
        let mut rows = view().flex_col();
        for i in 0..MODEL_SLOTS {
            let actions = rows_actions.clone();
            rows = rows.child(when(
                move || nav.with(|n| n.models.len() > i),
                move || model_row(nav, i, actions.clone()),
            ));
        }
        rows
    };
    view()
        .flex_col()
        .gap(px(16.0))
        .child(ui::search_field(move || nav.with(|n| n.filter.clone()), FILTER_LABEL))
        .child(when(
            move || nav.with(|n| n.save_default.is_some()),
            move || {
                let save_actions = save_actions.clone();
                ui::button(SAVE_DEFAULT_LABEL, Variant::Outline, Size::Sm, move || {
                    if let Some(action) = nav.with(|n| n.save_default.clone()) {
                        let _ = save_actions.send(action);
                    }
                })
            },
        ))
        .child(when(
            move || nav.with(|n| n.models_total == 0),
            move || ui::caption(MODELS_EMPTY_LABEL),
        ))
        .child(when(move || nav.with(|n| n.models_total > 0), move || ui::card_list(rows())))
}

fn model_row(
    nav: Signal<NavView>,
    i: usize,
    actions: UnboundedSender<UiAction>,
) -> impl IntoElement {
    let label = move || nav.with(|n| n.models.get(i).map(|r| r.text.clone()).unwrap_or_default());
    ui::choice_row(
        PICKER_CURRENT.trim(),
        move || nav.with(|n| n.models.get(i).is_some_and(|r| r.current)),
        move || nav.with(|n| n.selected == i),
        move || nav.with(|n| n.models.get(i).is_some_and(|r| !r.available)),
        label,
        move || {
            if let Some(model) = nav.with(|n| n.models.get(i).map(|r| r.model.clone())) {
                let _ = actions.send(UiAction::Nav(NavStep::PickModel(model)));
            }
        },
    )
}

/// The help screen: the three tabs over a window of the rows, and a footer
/// that says which rows show.
pub(crate) fn help(nav: Signal<NavView>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    let mut tabs = view().flex_row().w_full();
    for &kind in shown_tabs() {
        let actions = actions.clone();
        tabs = tabs.child(ui::tab(
            kind.label(),
            move || nav.with(|n| n.help_tab == kind),
            move || {
                let _ = actions.send(UiAction::Nav(NavStep::HelpTo(kind)));
            },
        ));
    }
    let rows = move || {
        let mut rows = view().flex_col();
        for i in 0..HELP_VISIBLE {
            rows = rows.child(when(
                move || nav.with(|n| n.help_rows.len() > n.help_scroll + i),
                move || {
                    ui::static_row(
                        view().flex_1().child(ui::body(move || {
                            nav.with(|n| {
                                n.help_rows.get(n.help_scroll + i).cloned().unwrap_or_default()
                            })
                        })),
                        None,
                    )
                },
            ));
        }
        rows
    };
    view().flex_col().gap(px(16.0)).child(ui::tab_list(tabs)).child(ui::card_list(rows())).child(
        ui::caption(move || {
            nav.with(|n| {
                let total = n.help_rows.len();
                let first = if total == 0 { 0 } else { n.help_scroll + 1 };
                let last = (n.help_scroll + HELP_VISIBLE).min(total);
                format!("{}: {first}-{last} of {total}", n.help_tab.label().to_lowercase())
            })
        }),
    )
}

/// The sessions screen (the local list): the filter, the rows (a tap attaches,
/// a delete control asks first), the new-session control and the confirmation.
pub(crate) fn sessions(
    nav: Signal<NavView>,
    actions: UnboundedSender<UiAction>,
) -> impl IntoElement {
    let new_actions = actions.clone();
    let rows_actions = actions.clone();
    let rows = move || {
        let mut rows = view().flex_col();
        for i in 0..SESSION_SLOTS {
            let actions = rows_actions.clone();
            rows = rows.child(when(
                move || nav.with(|n| n.sessions.len() > i),
                move || session_row(nav, i, actions.clone()),
            ));
        }
        rows
    };
    view()
        .flex_col()
        .gap(px(16.0))
        .child(ui::search_field(move || nav.with(|n| n.filter.clone()), FILTER_LABEL))
        .child(when(
            move || nav.with(|n| n.total == 0),
            move || ui::empty_state(SESSIONS_EMPTY_LABEL, "", None),
        ))
        .child(when(
            move || nav.with(|n| n.total == 0 && n.sessions_note.is_some()),
            move || ui::caption(move || nav.with(|n| n.sessions_note.clone().unwrap_or_default())),
        ))
        .child(when(
            move || nav.with(|n| n.total > 0 && n.sessions.is_empty()),
            move || ui::empty_state(SESSIONS_NO_MATCH_LABEL, "", None),
        ))
        .child(when(move || nav.with(|n| !n.sessions.is_empty()), move || ui::card_list(rows())))
        .child(
            ui::button(NEW_SESSION_LABEL, Variant::Outline, Size::Default, move || {
                let _ = new_actions.send(UiAction::Nav(NavStep::NewSession));
            })
            .w_full(),
        )
        .child(confirm(nav, actions))
}

fn session_row(
    nav: Signal<NavView>,
    i: usize,
    actions: UnboundedSender<UiAction>,
) -> impl IntoElement {
    let line = move || nav.with(|n| n.sessions.get(i).map(|r| r.text.clone()).unwrap_or_default());
    let delete = ui::button(DELETE_LABEL, Variant::Destructive, Size::Sm, {
        let actions = actions.clone();
        move || {
            let _ = actions.send(UiAction::Nav(NavStep::AskDelete(i)));
        }
    });
    ui::row(view().flex_1().child(ui::body(line)), Some(ui::trailing(delete, true)), move || {
        if let Some(id) = nav.with(|n| n.sessions.get(i).map(|r| r.id)) {
            let _ = actions.send(UiAction::Nav(NavStep::Attach(id)));
        }
    })
}

/// The tasks screen: the filter, the rows (a tap attaches; the merge control
/// asks first), the create control, the form and the confirmation.
pub(crate) fn tasks(nav: Signal<NavView>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    let new_actions = actions.clone();
    let rows_actions = actions.clone();
    let rows = move || {
        let mut rows = view().flex_col();
        for i in 0..TASK_SLOTS {
            let actions = rows_actions.clone();
            rows = rows.child(when(
                move || nav.with(|n| n.tasks.len() > i),
                move || task_row(nav, i, actions.clone()),
            ));
        }
        rows
    };
    view()
        .flex_col()
        .gap(px(16.0))
        .child(ui::search_field(move || nav.with(|n| n.filter.clone()), FILTER_LABEL))
        .child(when(
            move || nav.with(|n| n.tasks_total == 0),
            move || ui::empty_state(TASKS_EMPTY_LABEL, "", None),
        ))
        .child(when(
            move || nav.with(|n| n.tasks_total > 0 && n.tasks.is_empty()),
            move || ui::empty_state(TASKS_NO_MATCH_LABEL, "", None),
        ))
        .child(when(move || nav.with(|n| !n.tasks.is_empty()), move || ui::card_list(rows())))
        .child(when(
            move || nav.with(|n| n.note.is_some()),
            move || ui::caption(move || nav.with(|n| n.note.clone().unwrap_or_default())),
        ))
        .child(when(
            move || nav.with(|n| n.form.is_none()),
            move || {
                let new_actions = new_actions.clone();
                ui::button(TASK_NEW_LABEL, Variant::Outline, Size::Default, move || {
                    let _ = new_actions
                        .send(UiAction::Nav(NavStep::OpenForm(crate::nav::FormKind::Task)));
                })
                .w_full()
            },
        ))
        .child(form_card(nav, actions.clone()))
        .child(confirm(nav, actions))
}

fn task_row(
    nav: Signal<NavView>,
    i: usize,
    actions: UnboundedSender<UiAction>,
) -> impl IntoElement {
    let title = move || nav.with(|n| n.tasks.get(i).map(|r| r.text.clone()).unwrap_or_default());
    let summary =
        move || nav.with(|n| n.tasks.get(i).map(|r| r.summary.clone()).unwrap_or_default());
    let merge = ui::button(TASK_MERGE_LABEL, Variant::Outline, Size::Sm, {
        let actions = actions.clone();
        move || {
            let _ = actions.send(UiAction::Nav(NavStep::AskMerge(i)));
        }
    });
    let content = view().flex_col().gap(px(2.0)).flex_1().child(ui::body(title)).child(when(
        move || nav.with(|n| n.tasks.get(i).is_some_and(|r| !r.summary.is_empty())),
        move || ui::caption(summary),
    ));
    ui::row(content, Some(ui::trailing(merge, true)), move || {
        if let Some(id) = nav.with(|n| n.tasks.get(i).map(|r| r.id)) {
            let _ = actions.send(UiAction::Nav(NavStep::Attach(id)));
        }
    })
}

/// The automations screen: the filter, the rows (each with a removal control
/// that asks first), the add control, the form and the confirmation.
pub(crate) fn automations(
    nav: Signal<NavView>,
    actions: UnboundedSender<UiAction>,
) -> impl IntoElement {
    let add_actions = actions.clone();
    let rows_actions = actions.clone();
    let rows = move || {
        let mut rows = view().flex_col();
        for i in 0..AUTOMATION_SLOTS {
            let actions = rows_actions.clone();
            rows = rows.child(when(
                move || nav.with(|n| n.automations.len() > i),
                move || automation_row(nav, i, actions.clone()),
            ));
        }
        rows
    };
    view()
        .flex_col()
        .gap(px(16.0))
        .child(ui::search_field(move || nav.with(|n| n.filter.clone()), FILTER_LABEL))
        .child(when(
            move || nav.with(|n| n.automations_total == 0),
            move || ui::empty_state(AUTOMATIONS_EMPTY_LABEL, "", None),
        ))
        .child(when(
            move || nav.with(|n| n.automations_total > 0 && n.automations.is_empty()),
            move || ui::empty_state(AUTOMATIONS_NO_MATCH_LABEL, "", None),
        ))
        .child(when(move || nav.with(|n| !n.automations.is_empty()), move || ui::card_list(rows())))
        .child(when(
            move || nav.with(|n| n.form.is_none()),
            move || {
                let add_actions = add_actions.clone();
                ui::button(AUTOMATION_ADD_LABEL, Variant::Outline, Size::Default, move || {
                    let _ = add_actions
                        .send(UiAction::Nav(NavStep::OpenForm(crate::nav::FormKind::Automation)));
                })
                .w_full()
            },
        ))
        .child(form_card(nav, actions.clone()))
        .child(confirm(nav, actions))
}

fn automation_row(
    nav: Signal<NavView>,
    i: usize,
    actions: UnboundedSender<UiAction>,
) -> impl IntoElement {
    let line =
        move || nav.with(|n| n.automations.get(i).map(|r| r.text.clone()).unwrap_or_default());
    let delete = ui::button(DELETE_LABEL, Variant::Destructive, Size::Sm, move || {
        let _ = actions.send(UiAction::Nav(NavStep::AskRemove(i)));
    });
    ui::static_row(view().flex_1().child(ui::body(line)), Some(ui::trailing(delete, true)))
}

/// The open inline form (automations and tasks): one field per line, the
/// refusal, and the save and cancel controls. The form's keyboard hint is not
/// drawn: the two buttons are the same actions.
fn form_card(nav: Signal<NavView>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    when(
        move || nav.with(|n| n.form.is_some()),
        move || {
            let mut fields = view().flex_col().gap(px(16.0));
            for i in 0..FORM_SLOTS {
                fields = fields.child(when(
                    move || nav.with(|n| n.form.as_ref().is_some_and(|f| f.labels.len() > i)),
                    move || {
                        ui::field(
                            move || {
                                nav.with(|n| {
                                    n.form
                                        .as_ref()
                                        .and_then(|f| f.labels.get(i))
                                        .map(|l| l.to_string())
                                        .unwrap_or_default()
                                })
                            },
                            move || {
                                nav.with(|n| {
                                    n.form
                                        .as_ref()
                                        .and_then(|f| f.values.get(i).cloned())
                                        .unwrap_or_default()
                                })
                            },
                            move || nav.with(|n| n.form.as_ref().is_some_and(|f| f.active == i)),
                        )
                    },
                ));
            }
            let error = when(
                move || nav.with(|n| n.form.as_ref().and_then(|f| f.error.clone()).is_some()),
                move || {
                    ui::alert(Tone::Destructive, move || {
                        nav.with(|n| {
                            n.form.as_ref().and_then(|f| f.error.clone()).unwrap_or_default()
                        })
                    })
                    .w_full()
                },
            );
            let save = actions.clone();
            let cancel = actions.clone();
            let buttons = view()
                .flex_row()
                .gap(px(8.0))
                .child(ui::button(FORM_SAVE_LABEL, Variant::Default, Size::Sm, move || {
                    let _ = save.send(UiAction::Nav(NavStep::FormSubmit));
                }))
                .child(ui::button(FORM_CANCEL_LABEL, Variant::Outline, Size::Sm, move || {
                    let _ = cancel.send(UiAction::Nav(NavStep::FormCancel));
                }));
            ui::card(ui::card_content(fields.child(error).child(buttons)))
        },
    )
}

/// The Cloud screen (owner): the note, the linked GitHub accounts (add by
/// token, remove with a confirmation), the new-run form (account, repo,
/// branch, start) and the runs (open, stop, wake).
pub(crate) fn cloud(nav: Signal<NavView>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    let accounts_actions = actions.clone();
    let token_actions = actions.clone();
    let run_actions = actions.clone();
    let start_actions = actions.clone();
    let runs_actions = actions.clone();
    view()
        .flex_col()
        .gap(px(24.0))
        .child(when(
            move || nav.with(|n| n.cloud.note.is_some()),
            move || {
                ui::alert(Tone::Muted, move || {
                    nav.with(|n| n.cloud.note.clone().unwrap_or_default())
                })
                .w_full()
            },
        ))
        .child(
            view()
                .flex_col()
                .gap(px(16.0))
                .child(ui::section_title(CLOUD_ACCOUNTS_LABEL))
                .child(when(
                    move || nav.with(|n| n.cloud.accounts.is_empty()),
                    move || ui::caption(CLOUD_ACCOUNTS_EMPTY_LABEL),
                ))
                .child(when(
                    move || nav.with(|n| !n.cloud.accounts.is_empty()),
                    move || {
                        let mut rows = view().flex_col();
                        for i in 0..CLOUD_ACCOUNT_SLOTS {
                            let actions = accounts_actions.clone();
                            rows = rows.child(when(
                                move || nav.with(|n| n.cloud.accounts.len() > i),
                                move || cloud_account_row(nav, i, actions.clone()),
                            ));
                        }
                        ui::card_list(rows)
                    },
                ))
                .child(when(
                    move || nav.with(|n| !n.cloud.adding),
                    move || {
                        let token_actions = token_actions.clone();
                        ui::button(CLOUD_ACCOUNT_ADD_LABEL, Variant::Outline, Size::Sm, move || {
                            let _ = token_actions.send(UiAction::Nav(NavStep::CloudAddOpen));
                        })
                    },
                ))
                .child(when(
                    move || nav.with(|n| n.cloud.adding),
                    move || cloud_token_entry(run_actions.clone()),
                )),
        )
        .child(
            view()
                .flex_col()
                .gap(px(16.0))
                .child(ui::section_title(CLOUD_RUN_LABEL))
                .child(ui::card(ui::card_content(new_run(nav, start_actions.clone())))),
        )
        .child(
            view()
                .flex_col()
                .gap(px(16.0))
                .child(ui::section_title(CLOUD_RUNS_LABEL))
                .child(when(
                    move || nav.with(|n| n.cloud.runs.is_empty()),
                    move || ui::caption(CLOUD_RUNS_EMPTY_LABEL),
                ))
                .child(when(
                    move || nav.with(|n| !n.cloud.runs.is_empty()),
                    move || {
                        let mut rows = view().flex_col();
                        for i in 0..CLOUD_RUN_SLOTS {
                            let actions = runs_actions.clone();
                            rows = rows.child(when(
                                move || nav.with(|n| n.cloud.runs.len() > i),
                                move || cloud_run_row(nav, i, actions.clone()),
                            ));
                        }
                        ui::card_list(rows)
                    },
                )),
        )
        .child(confirm(nav, actions))
}

/// One linked account: its line, and a removal control that asks first.
fn cloud_account_row(
    nav: Signal<NavView>,
    i: usize,
    actions: UnboundedSender<UiAction>,
) -> impl IntoElement {
    let line =
        move || nav.with(|n| n.cloud.accounts.get(i).map(|a| a.text.clone()).unwrap_or_default());
    let remove = ui::button(REMOVE_LABEL, Variant::Destructive, Size::Sm, move || {
        let _ = actions.send(UiAction::Nav(NavStep::AskAccountRemove(i)));
    });
    ui::static_row(view().flex_1().child(ui::body(line)), Some(ui::trailing(remove, true)))
}

/// The new-run controls: the account picker (with more than one account), the
/// repo picker, the branch field (once a repo is chosen) and the start control.
fn new_run(nav: Signal<NavView>, actions: UnboundedSender<UiAction>) -> impl IntoElement {
    let pick_actions = actions.clone();
    let repo_actions = actions.clone();
    let branch_actions = actions.clone();
    let start_actions = actions;
    view()
        .flex_col()
        .gap(px(16.0))
        .child(when(
            move || nav.with(|n| n.cloud.accounts.is_empty()),
            move || ui::caption(CLOUD_NO_ACCOUNT_LABEL),
        ))
        .child(when(
            move || nav.with(|n| n.cloud.accounts.len() > 1),
            move || {
                let pick_actions = pick_actions.clone();
                ui::button_text(
                    move || {
                        nav.with(|n| {
                            let login = n
                                .cloud
                                .chosen_account()
                                .map(|a| a.login.clone())
                                .unwrap_or_default();
                            format!("{CLOUD_ACCOUNT_PICK_LABEL}: {login}")
                        })
                    },
                    Variant::Outline,
                    Size::Default,
                    move || {
                        let _ = pick_actions.send(UiAction::Nav(NavStep::CloudPickAccount));
                    },
                )
                .w_full()
            },
        ))
        .child(when(
            move || nav.with(|n| !n.cloud.accounts.is_empty()),
            move || {
                let repo_actions = repo_actions.clone();
                ui::button_text(
                    move || {
                        nav.with(|n| match &n.cloud.repo {
                            Some(repo) => format!("{CLOUD_REPO_PICK_LABEL}: {}", repo.full_name),
                            None => CLOUD_REPO_PICK_LABEL.to_string(),
                        })
                    },
                    Variant::Outline,
                    Size::Default,
                    move || {
                        let _ = repo_actions.send(UiAction::Nav(NavStep::Open(Screen::CloudRepos)));
                    },
                )
                .w_full()
            },
        ))
        .child(when(
            move || nav.with(|n| n.cloud.repo.is_some()),
            move || {
                let branch_actions = branch_actions.clone();
                ui::text_field(
                    "",
                    "",
                    input()
                        .placeholder(CLOUD_BRANCH_LABEL)
                        .value(move || nav.with(|n| n.cloud.branch.clone()))
                        .on_change(move |s: &str| {
                            let _ = branch_actions
                                .send(UiAction::Nav(NavStep::SetBranch(s.to_string())));
                        }),
                )
            },
        ))
        .child(when(
            move || nav.with(|n| n.cloud_start_action().is_some()),
            move || {
                let start_actions = start_actions.clone();
                ui::button(CLOUD_START_LABEL, Variant::Default, Size::Default, move || {
                    if let Some(action) = nav.with(|n| n.cloud_start_action()) {
                        let _ = start_actions.send(action);
                    }
                })
                .w_full()
            },
        ))
}

/// One run: its line, then open (when it has a session), stop and wake as its
/// state offers them.
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
    let buttons = view()
        .flex_row()
        .flex_wrap()
        .gap(px(8.0))
        .child(when(
            move || nav.with(|n| n.cloud.runs.get(i).is_some_and(|r| !r.session_url.is_empty())),
            move || {
                let open_actions = open_actions.clone();
                ui::button(CLOUD_OPEN_LABEL, Variant::Outline, Size::Sm, move || {
                    let _ = open_actions.send(UiAction::Nav(NavStep::CloudOpenRun(i)));
                })
            },
        ))
        .child(when(
            move || nav.with(|n| n.cloud.runs.get(i).is_some_and(|r| r.stop)),
            move || {
                let stop_actions = stop_actions.clone();
                ui::button(CLOUD_STOP_LABEL, Variant::Destructive, Size::Sm, move || {
                    if let Some(id) = nav.with(|n| n.cloud.runs.get(i).map(|r| r.id.clone())) {
                        let _ = stop_actions.send(UiAction::StopRun(id));
                    }
                })
            },
        ))
        .child(when(
            move || nav.with(|n| n.cloud.runs.get(i).is_some_and(|r| r.wake)),
            move || {
                let wake_actions = wake_actions.clone();
                ui::button(CLOUD_WAKE_LABEL, Variant::Secondary, Size::Sm, move || {
                    if let Some(id) = nav.with(|n| n.cloud.runs.get(i).map(|r| r.id.clone())) {
                        let _ = wake_actions.send(UiAction::WakeRun(id));
                    }
                })
            },
        ));
    ui::static_row(
        view().flex_col().gap(px(8.0)).flex_1().child(ui::body(line)).child(buttons),
        None,
    )
}

/// The masked token entry for a new linked account. The field is cleared on
/// every submit; Enter and the save control send the same action.
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
    let save = ui::button(FORM_SAVE_LABEL, Variant::Default, Size::Default, move || {
        let value = token.get();
        token.set(String::new());
        send_token(&press, &value);
    })
    .w_full();
    ui::card(ui::card_content(
        view()
            .flex_col()
            .gap(px(12.0))
            .child(ui::caption(CLOUD_TOKEN_HINT))
            .child(ui::text_field("", "", field))
            .child(save),
    ))
}

/// The repo picker (owner): the filter (sent to the server as `q`), the rows,
/// and the empty state. A tap on a row uses that repo.
pub(crate) fn cloud_repos(
    nav: Signal<NavView>,
    actions: UnboundedSender<UiAction>,
) -> impl IntoElement {
    let rows_actions = actions.clone();
    let rows = move || {
        let mut rows = view().flex_col();
        for i in 0..CLOUD_REPO_SLOTS {
            let actions = rows_actions.clone();
            rows = rows.child(when(
                move || nav.with(|n| n.cloud.repos.len() > i),
                move || {
                    let actions = actions.clone();
                    ui::row(
                        view().flex_1().child(ui::body(move || {
                            nav.with(|n| {
                                n.cloud.repos.get(i).map(|r| r.text.clone()).unwrap_or_default()
                            })
                        })),
                        None,
                        move || {
                            let _ = actions.send(UiAction::Nav(NavStep::PickRepo(i)));
                        },
                    )
                },
            ));
        }
        rows
    };
    view()
        .flex_col()
        .gap(px(16.0))
        .child(ui::search_field(move || nav.with(|n| n.filter.clone()), FILTER_LABEL))
        .child(when(
            move || nav.with(|n| n.cloud.repos.is_empty() && n.cloud.next_page.is_none()),
            move || ui::empty_state(CLOUD_REPOS_EMPTY_LABEL, "", None),
        ))
        .child(when(move || nav.with(|n| !n.cloud.repos.is_empty()), move || ui::card_list(rows())))
}

/// The removal, cancel and other confirmations: the shared line, and the yes
/// (destructive) and no controls. Shown only while a confirmation waits.
pub(crate) fn confirm(
    nav: Signal<NavView>,
    actions: UnboundedSender<UiAction>,
) -> impl IntoElement {
    when(
        move || nav.with(|n| n.confirm.is_some()),
        move || {
            let yes = actions.clone();
            let no = actions.clone();
            ui::confirm_dialog(
                move || nav.with(|n| confirm_question(&n.confirm.clone().unwrap_or_default())),
                "",
                CONFIRM_YES_LABEL,
                CONFIRM_NO_LABEL,
                move || {
                    let _ = yes.send(UiAction::Nav(NavStep::Confirm));
                },
                move || {
                    let _ = no.send(UiAction::Nav(NavStep::Cancel));
                },
            )
        },
    )
}

/// The question a confirmation asks, as the web's dialog title: the shared
/// sentence without its terminal key hint (` y / n`), which the dialog's two
/// buttons replace.
fn confirm_question(text: &str) -> String {
    let hint = format!(" {CONFIRM_YES_LABEL} / {CONFIRM_NO_LABEL}");
    text.strip_suffix(&hint).unwrap_or(text).to_string()
}

/// The sub-agent panel (web), above the transcript while the agent screen is
/// open: the agent's row text and its state, the kill control while it runs,
/// and the confirmation.
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
        move || nav.with(|n| n.agent_live),
        move || {
            let kill_actions = kill_actions.clone();
            ui::button(AGENT_KILL_LABEL, Variant::Destructive, Size::Sm, move || {
                let _ = kill_actions.send(UiAction::Nav(NavStep::AskKill));
            })
        },
    );
    ui::gutter(view().flex_col().gap(px(8.0)).py(px(8.0)).w_full())
        .child(ui::card_list(ui::static_row(
            view()
                .flex_col()
                .gap(px(2.0))
                .flex_1()
                .child(ui::body(title).truncate())
                .child(ui::caption(state)),
            Some(kill.into_element()),
        )))
        .child(confirm(nav, actions))
}
