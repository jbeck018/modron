//! The key table (Packet C3f): ONE list of the app's bindings. The help
//! screen prints it, and the key-router test iterates it, so help cannot
//! drift from behaviour. Slash commands come from [`crate::slash::all`].

use modron_mobile_core::view::WELCOME_TAGLINE;
use modron_view::slash::{slash_help_lines, slash_lookup};
use uuid::Uuid;

use crate::chat::{Key, KeyFacts, Routed, ScrollStep, ToolsStep, UiAction};
use crate::nav::{FormKind, NavStep, Screen};
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
            label: "a (automations)",
            what: "opens the add form: name, prompt, interval minutes (tab next, enter save, esc cancel)",
            key: Key::Char('a'),
            chord: false,
            alt: false,
            facts: || {
                with(|f| {
                    f.screen = Screen::Automations;
                })
            },
            draft: "",
            expect: || Routed::Nav(NavStep::OpenForm(FormKind::Automation)),
        },
        Binding {
            label: "d (automations)",
            what: "asks to remove the selected automation (y removes, n keeps it)",
            key: Key::Char('d'),
            chord: false,
            alt: false,
            facts: || {
                with(|f| {
                    f.screen = Screen::Automations;
                })
            },
            draft: "",
            expect: || Routed::Nav(NavStep::AskRemove(0)),
        },
        Binding {
            label: "n (tasks)",
            what: "opens the new task form (its title; enter creates it in a worktree)",
            key: Key::Char('n'),
            chord: false,
            alt: false,
            facts: || {
                with(|f| {
                    f.screen = Screen::Tasks;
                })
            },
            draft: "",
            expect: || Routed::Nav(NavStep::OpenForm(FormKind::Task)),
        },
        Binding {
            label: "m (tasks)",
            what: "asks to merge the selected task's branch into its base (y merges, n keeps it)",
            key: Key::Char('m'),
            chord: false,
            alt: false,
            facts: || {
                with(|f| {
                    f.screen = Screen::Tasks;
                })
            },
            draft: "",
            expect: || Routed::Nav(NavStep::AskMerge(0)),
        },
        Binding {
            label: "enter (tasks)",
            what: "attaches the selected task's session and returns to chat",
            key: Key::Enter,
            chord: false,
            alt: false,
            facts: || {
                with(|f| {
                    f.screen = Screen::Tasks;
                    f.selected_task = Some(session_id());
                })
            },
            draft: "",
            expect: || Routed::Nav(NavStep::Attach(session_id())),
        },
        Binding {
            label: "ctrl+v",
            what: "paste an image from the clipboard as a chip for the next message",
            key: Key::Char('v'),
            chord: true,
            alt: false,
            facts: chat,
            draft: "",
            expect: || Routed::Action(UiAction::Paste),
        },
        Binding {
            label: "shift+s (model picker)",
            what: "saves the highlighted model and provider as the default for new sessions",
            key: Key::Char('S'),
            chord: false,
            alt: false,
            facts: || {
                with(|f| {
                    f.screen = Screen::Models;
                    f.save_default = Some(UiAction::SaveDefaultModel {
                        model: "m".into(),
                        provider: "p".into(),
                    });
                })
            },
            draft: "",
            expect: || {
                Routed::Action(UiAction::SaveDefaultModel {
                    model: "m".into(),
                    provider: "p".into(),
                })
            },
        },
        Binding {
            label: "ctrl+t",
            what: "tool detail panel: this turn's tool rows (esc or ctrl+t closes)",
            key: Key::Char('t'),
            chord: true,
            alt: false,
            facts: chat,
            draft: "",
            expect: || Routed::Tools(ToolsStep::Toggle),
        },
        Binding {
            label: "esc (tool panel)",
            what: "close the tool detail panel; up/down, pgup/pgdn, home/end scroll it",
            key: Key::Escape,
            chord: false,
            alt: false,
            facts: || with(|f| f.tool_panel = true),
            draft: "",
            expect: || Routed::Tools(ToolsStep::Close),
        },
        Binding {
            label: "tab (@file)",
            what:
                "complete the selected @path (terminal only; the list is the same as for /commands)",
            key: Key::Tab,
            chord: false,
            alt: false,
            facts: || {
                with(|f| {
                    f.slash_len = 1;
                    f.file_mode = true;
                })
            },
            draft: "@ma",
            expect: || Routed::CompleteFile,
        },
        Binding {
            label: "!command",
            what: "run the rest of the line in the shell (not the model); !! runs it privately (shown, never sent to the model)",
            key: Key::Enter,
            chord: false,
            alt: false,
            facts: chat,
            draft: "!ls",
            expect: || Routed::Composer,
        },
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
            label: "esc (composer)",
            what: "clear the composer text when nothing else takes esc (chips stay)",
            key: Key::Escape,
            chord: false,
            alt: false,
            facts: chat,
            draft: "hi",
            expect: || Routed::ClearDraft,
        },
        Binding {
            label: "esc",
            what: "deny the pending approval, else close suggestions, else interrupt the run",
            key: Key::Escape,
            chord: false,
            alt: false,
            facts: || with(|f| f.running = true),
            draft: "",
            expect: || Routed::Action(UiAction::Interrupt),
        },
        Binding {
            label: "shift+enter",
            what: "new line in the composer; ctrl+j and a line ending in \\ always do it, shift+enter where the terminal reports it",
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
            what: "remove the last image chip, else the last queued message (empty composer)",
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
        Binding {
            label: "d (settings)",
            what: "removes the selected permission rule or allow or deny entry (y confirms, n cancels)",
            key: Key::Char('d'),
            chord: false,
            alt: false,
            facts: || with(|f| {
                f.screen = Screen::Settings;
                f.removable = true;
            }),
            draft: "",
            expect: || Routed::Nav(NavStep::AskSettingRemove(0)),
        },
        Binding {
            label: "d (credentials)",
            what: "removes a signed-in provider's saved credentials (y confirms, n cancels); a row not signed in has no remove",
            key: Key::Char('d'),
            chord: false,
            alt: false,
            facts: || with(|f| f.screen = Screen::Auth),
            draft: "",
            expect: || Routed::Nav(NavStep::AskAuthRemove(0)),
        },
        Binding {
            label: "enter (pickers)",
            what: "applies the highlighted row of /mode, /sandbox, /effort, /agent or /theme and closes the list",
            key: Key::Enter,
            chord: false,
            alt: false,
            facts: || with(|f| f.screen = Screen::Picker),
            draft: "",
            expect: || Routed::Nav(NavStep::PickRow(0)),
        },
        Binding {
            label: "tab (help)",
            what: "moves to the next help tab (Shift+Tab or Left goes back; Up, Down, PgUp, PgDn, Home and End scroll)",
            key: Key::Tab,
            chord: false,
            alt: false,
            facts: || with(|f| f.screen = Screen::Help),
            draft: "",
            expect: || Routed::Nav(NavStep::HelpCycle(1)),
        },
    ]
}

/// The help body, as rendered lines: the keys section, then the slash
/// commands (the shared table rows, then the app's local rows).
/// Bindings the terminal host handles before the router. Help lists them;
/// the routing test does not (it cannot name a key event the router sees).
pub const HOST_BINDINGS: &[(&str, &str)] =
    &[("ctrl+c", "interrupt the running turn; a second press within 2 s, or when idle, quits")];

/// The help tabs, in order: the old terminal's General, Commands and
/// Shortcuts (crates/modron-tui/src/lib.rs, `HelpTab`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HelpTab {
    #[default]
    General,
    Commands,
    Shortcuts,
}

/// The tabs in display order.
pub const HELP_TABS: [HelpTab; 3] = [HelpTab::General, HelpTab::Commands, HelpTab::Shortcuts];

/// Rows of a help tab shown at once, and the rows a PgUp or PgDn moves.
pub const HELP_VISIBLE: usize = 14;
pub const HELP_PAGE: usize = 10;

impl HelpTab {
    pub fn label(self) -> &'static str {
        match self {
            HelpTab::General => "General",
            HelpTab::Commands => "Commands",
            HelpTab::Shortcuts => "Shortcuts",
        }
    }

    /// The tab `delta` steps away. The ring wraps at both ends, as the old
    /// terminal's arrows did.
    pub fn step(self, delta: isize) -> HelpTab {
        let at = HELP_TABS.iter().position(|t| *t == self).unwrap_or(0) as isize;
        HELP_TABS[(at + delta).rem_euclid(HELP_TABS.len() as isize) as usize]
    }
}

/// The tabs the web shows. Shortcuts lists keyboard bindings, which the web's
/// controls replace; the terminal keeps all three.
pub fn shown_tabs() -> &'static [HelpTab] {
    if crate::chat::WEB_CONTROLS {
        &[HelpTab::General, HelpTab::Commands]
    } else {
        &HELP_TABS
    }
}

/// The rows a tab shows. On the web General keeps its tagline only: its other
/// rows name terminal keys. The terminal shows [`tab_lines`] as they are.
pub fn rows_for(tab: HelpTab) -> Vec<String> {
    if !crate::chat::WEB_CONTROLS {
        return tab_lines(tab);
    }
    match tab {
        HelpTab::General => vec![WELCOME_TAGLINE.to_string()],
        HelpTab::Commands => tab_lines(tab),
        HelpTab::Shortcuts => Vec::new(),
    }
}

/// The rows of one tab. Commands come from the shared slash table and the
/// app's own rows; Shortcuts from the one key table, so help cannot drift
/// from behaviour.
pub fn tab_lines(tab: HelpTab) -> Vec<String> {
    match tab {
        HelpTab::General => vec![
            WELCOME_TAGLINE.to_string(),
            "Enter sends a task; Alt+Enter queues it behind the running turn.".to_string(),
            "Esc interrupts a running turn. Shift+Tab flips the approval mode.".to_string(),
            "/mode, /sandbox, /effort, /agent and /theme open a list: type to filter, Enter applies, Esc closes.".to_string(),
            "Tab or Right and Shift+Tab or Left switch tabs; Up and Down, PgUp and PgDn, Home and End scroll.".to_string(),
        ],
        HelpTab::Commands => {
            let mut lines: Vec<String> = Vec::new();
            lines.extend(slash_help_lines());
            // The app's own rows only: a name the shared table lists is already above.
            lines.extend(
                slash::all()
                    .iter()
                    .filter(|e| slash_lookup(&e.name).is_none())
                    .map(|e| format!("{} - {}", e.usage, e.about)),
            );
            lines
        }
        HelpTab::Shortcuts => {
            // The list screens' keys come last, as they always have.
            let (list_keys, keys): (Vec<Binding>, Vec<Binding>) = bindings()
                .into_iter()
                .partition(|b| matches!((b.facts)().screen, Screen::Automations | Screen::Tasks));
            let mut lines: Vec<String> =
                keys.iter().map(|b| format!("{}   {}", b.label, b.what)).collect();
            lines.extend(HOST_BINDINGS.iter().map(|(label, what)| format!("{label}   {what}")));
            lines.extend(list_keys.iter().map(|b| format!("{}   {}", b.label, b.what)));
            lines
        }
    }
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
        let shortcuts = tab_lines(HelpTab::Shortcuts);
        for b in bindings() {
            assert!(shortcuts.iter().any(|l| l.starts_with(b.label)), "{}", b.label);
        }
        let commands = tab_lines(HelpTab::Commands);
        for e in slash::all() {
            assert!(commands.iter().any(|l| l.starts_with(e.usage.as_ref())), "{}", e.usage);
        }
    }

    #[test]
    fn the_tabs_step_in_order_and_wrap_at_both_ends() {
        assert_eq!(HelpTab::General.step(1), HelpTab::Commands);
        assert_eq!(HelpTab::Commands.step(1), HelpTab::Shortcuts);
        assert_eq!(HelpTab::Shortcuts.step(1), HelpTab::General);
        assert_eq!(HelpTab::General.step(-1), HelpTab::Shortcuts);
        assert_eq!(HelpTab::Shortcuts.step(-4), HelpTab::Commands);
    }

    #[test]
    fn every_tab_has_rows_and_the_labels_are_the_old_three() {
        for tab in HELP_TABS {
            assert!(!tab_lines(tab).is_empty(), "{}", tab.label());
        }
        let labels: Vec<&str> = HELP_TABS.iter().map(|t| t.label()).collect();
        assert_eq!(labels, ["General", "Commands", "Shortcuts"]);
    }
}
