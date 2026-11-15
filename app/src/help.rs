//! The key table (Packet C3f): ONE list of the app's bindings. The help
//! screen prints it, and the key-router test iterates it, so help cannot
//! drift from behaviour. Slash commands come from [`crate::slash::all`].

use modron_mobile_core::view::{HELP_COMMANDS_HEADING, HELP_KEYS_HEADING};
use modron_view::slash::slash_help_lines;
use uuid::Uuid;

use crate::chat::{Key, KeyFacts, Routed, ScrollStep, UiAction};
use crate::nav::{NavStep, Screen};
use crate::slash;

/// One binding: the label and wording help shows, and a routing case the
/// tests check (`key`, `chord`, `alt`, the facts, the draft, the result).
pub struct Binding {
    pub label: &'static str,
    pub what: &'static str,
    pub key: Key,
    pub chord: bool,
    pub alt: bool,
    pub facts: fn() -> KeyFacts,
    pub draft: &'static str,
    pub expect: fn() -> Routed,
}

fn chat() -> KeyFacts {
    KeyFacts::default()
}

fn with(f: impl FnOnce(&mut KeyFacts)) -> KeyFacts {
    let mut facts = KeyFacts::default();
    f(&mut facts);
    facts
}

fn session_id() -> Uuid {
    Uuid::from_u128(0xd1)
}

/// Every binding the app has, in help order.
pub fn bindings() -> Vec<Binding> {
    vec![
        Binding {
            label: "enter",
            what: "send (steers while a run is active)",
            key: Key::Enter,
            chord: false,
            alt: false,
            facts: chat,
            draft: "hi",
            expect: || Routed::Composer,
        },
        Binding {
            label: "alt+enter",
            what: "queue the message for the next run",
            key: Key::Enter,
            chord: false,
            alt: true,
            facts: chat,
            draft: "hi",
            expect: || Routed::Action(UiAction::Queue("hi".into())),
        },
        Binding {
            label: "esc",
            what: "deny the pending approval, else interrupt the run",
            key: Key::Escape,
            chord: false,
            alt: false,
            facts: || with(|f| f.running = true),
            draft: "",
            expect: || Routed::Action(UiAction::Interrupt),
        },
        Binding {
            label: "shift+enter",
            what: "new line in the composer (ctrl+j too; a line ending in \\ does the same)",
            key: Key::Enter,
            chord: false,
            alt: false,
            facts: chat,
            draft: "first line",
            expect: || Routed::Composer,
        },
        Binding {
            label: "up / down (empty composer)",
            what: "recall the last sent input; down past the newest restores the draft",
            key: Key::Up,
            chord: false,
            alt: false,
            facts: || with(|f| f.history_len = 2),
            draft: "",
            expect: || Routed::Recall { up: true },
        },
        Binding {
            label: "shift+tab",
            what: "flip the approval mode (ask / yolo)",
            key: Key::BackTab,
            chord: false,
            alt: false,
            facts: chat,
            draft: "",
            expect: || Routed::Action(UiAction::CycleMode),
        },
        Binding {
            label: "pgup / pgdn",
            what: "page the transcript; new output does not move a scrolled view",
            key: Key::PageUp,
            chord: false,
            alt: false,
            facts: chat,
            draft: "",
            expect: || Routed::Scroll(ScrollStep::Up),
        },
        Binding {
            label: "home / end (empty composer)",
            what: "jump to the top of the transcript / back to the newest row",
            key: Key::Home,
            chord: false,
            alt: false,
            facts: chat,
            draft: "",
            expect: || Routed::Scroll(ScrollStep::Top),
        },
        Binding {
            label: "y / a / n",
            what: "decide the pending approval",
            key: Key::Char('y'),
            chord: false,
            alt: false,
            facts: || with(|f| f.approval_options = Some(3)),
            draft: "",
            expect: || Routed::Action(UiAction::Decide(0)),
        },
        Binding {
            label: "y / r",
            what: "approve or revise the proposed plan",
            key: Key::Char('r'),
            chord: false,
            alt: false,
            facts: || with(|f| f.plan_pending = true),
            draft: "split it",
            expect: || {
                Routed::Action(UiAction::DecidePlan {
                    approve: false,
                    note: Some("split it".into()),
                })
            },
        },
        Binding {
            label: "backspace",
            what: "remove the last queued message (empty composer)",
            key: Key::Backspace,
            chord: false,
            alt: false,
            facts: || with(|f| f.queued = 2),
            draft: "",
            expect: || Routed::Action(UiAction::Dequeue(1)),
        },
        Binding {
            label: "up / down",
            what: "move in the command suggestions",
            key: Key::Down,
            chord: false,
            alt: false,
            facts: || with(|f| f.slash_len = 2),
            draft: "/",
            expect: || Routed::Nav(NavStep::SlashMove(1)),
        },
        Binding {
            label: "tab",
            what: "complete the selected command",
            key: Key::Tab,
            chord: false,
            alt: false,
            facts: || with(|f| f.slash_len = 2),
            draft: "/mo",
            expect: || Routed::Nav(NavStep::CompleteSelected),
        },
        Binding {
            label: "enter (command)",
            what: "complete a partial command, or run it when complete",
            key: Key::Enter,
            chord: false,
            alt: false,
            facts: || with(|f| f.slash_len = 2),
            draft: "/mo",
            expect: || Routed::Nav(NavStep::CompleteSelected),
        },
        Binding {
            label: "esc (command)",
            what: "hide the command suggestions (text stays)",
            key: Key::Escape,
            chord: false,
            alt: false,
            facts: || with(|f| f.slash_len = 2),
            draft: "/mo",
            expect: || Routed::Nav(NavStep::SlashHide),
        },
        Binding {
            label: "?",
            what: "help (empty composer)",
            key: Key::Char('?'),
            chord: false,
            alt: false,
            facts: chat,
            draft: "",
            expect: || Routed::Nav(NavStep::Open(Screen::Help)),
        },
        Binding {
            label: "f1",
            what: "help",
            key: Key::F1,
            chord: false,
            alt: false,
            facts: chat,
            draft: "",
            expect: || Routed::Nav(NavStep::Open(Screen::Help)),
        },
        Binding {
            label: "f2",
            what: "settings",
            key: Key::F2,
            chord: false,
            alt: false,
            facts: chat,
            draft: "",
            expect: || Routed::Nav(NavStep::Open(Screen::Settings)),
        },
        Binding {
            label: "ctrl+o",
            what: "sessions",
            key: Key::Char('o'),
            chord: true,
            alt: false,
            facts: chat,
            draft: "",
            expect: || Routed::Nav(NavStep::Open(Screen::Sessions)),
        },
        Binding {
            label: "ctrl+a",
            what: "open a sub-agent (its steer and kill controls)",
            key: Key::Char('a'),
            chord: true,
            alt: false,
            facts: || with(|f| f.first_agent = Some("agent-1".into())),
            draft: "",
            expect: || Routed::Nav(NavStep::OpenAgent("agent-1".into())),
        },
        Binding {
            label: "d",
            what: "delete the selected session (filter empty)",
            key: Key::Char('d'),
            chord: false,
            alt: false,
            facts: || {
                with(|f| {
                    f.screen = Screen::Sessions;
                    f.selected_session = Some(session_id());
                })
            },
            draft: "",
            expect: || Routed::Nav(NavStep::AskDelete(0)),
        },
        Binding {
            label: "enter (model)",
            what: "switch to the selected model",
            key: Key::Enter,
            chord: false,
            alt: false,
            facts: || {
                with(|f| {
                    f.screen = Screen::Models;
                    f.selected_model = Some("claude-haiku-5-5".into());
                })
            },
            draft: "",
            expect: || Routed::Nav(NavStep::PickModel("claude-haiku-5-5".into())),
        },
        Binding {
            label: "k",
            what: "kill the open sub-agent (empty steer text)",
            key: Key::Char('k'),
            chord: false,
            alt: false,
            facts: || {
                with(|f| {
                    f.screen = Screen::Agent;
                    f.agent_live = true;
                })
            },
            draft: "",
            expect: || Routed::Nav(NavStep::AskKill),
        },
        Binding {
            label: "y / n",
            what: "confirm or cancel the pending delete or kill",
            key: Key::Char('y'),
            chord: false,
            alt: false,
            facts: || with(|f| f.confirming = true),
            draft: "",
            expect: || Routed::Nav(NavStep::Confirm),
        },
        Binding {
            label: "esc (screen)",
            what: "close this screen (clears a filter first)",
            key: Key::Escape,
            chord: false,
            alt: false,
            facts: || with(|f| f.screen = Screen::Help),
            draft: "",
            expect: || Routed::Nav(NavStep::Back),
        },
    ]
}

/// The help body, as rendered lines: the keys section, then the slash
/// commands (the shared table rows, then the app's local rows).
/// Bindings the terminal host handles before the router. Help lists them;
/// the routing test does not (it cannot name a key event the router sees).
pub const HOST_BINDINGS: &[(&str, &str)] =
    &[("ctrl+c", "interrupt the running turn; a second press within 2 s, or when idle, quits")];

pub fn help_lines() -> Vec<String> {
    let mut lines = vec![HELP_KEYS_HEADING.to_string()];
    lines.extend(bindings().iter().map(|b| format!("{}   {}", b.label, b.what)));
    lines.extend(HOST_BINDINGS.iter().map(|(label, what)| format!("{label}   {what}")));
    lines.push(HELP_COMMANDS_HEADING.to_string());
    lines.extend(slash_help_lines());
    lines.extend(slash::LOCAL.iter().map(|e| format!("{} - {}", e.usage, e.about)));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::route_key;

    #[test]
    fn every_binding_routes_as_the_table_says() {
        for b in bindings() {
            let got = route_key(b.key, b.alt, b.chord, (b.facts)(), b.draft);
            assert_eq!(got, (b.expect)(), "binding {}", b.label);
        }
    }

    #[test]
    fn help_lists_every_binding_and_every_command() {
        let lines = help_lines();
        for b in bindings() {
            assert!(lines.iter().any(|l| l.starts_with(b.label)), "{}", b.label);
        }
        for e in slash::all() {
            assert!(lines.iter().any(|l| l.starts_with(e.usage)), "{}", e.usage);
        }
    }
}
