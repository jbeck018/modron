//! Slash commands in the composer (Packet C3f). Target-neutral.
//!
//! The names, usage lines and descriptions come from the shared table
//! (`modron_view::slash::SLASH_COMMANDS`). [`LOCAL`] lists the commands this
//! app serves beyond that table (the table has no `compact`, `effort`,
//! `plan`, `build` or `sessions` yet); they appear in the suggestions and in
//! help, after the table rows.
//!
//! [`run`] maps one submitted line to one [`UiAction`]: a screen step
//! (`UiAction::Nav`) for the commands that open a screen, a core call
//! otherwise, and a notice for an unknown word or a missing argument. Every
//! table command has a branch in [`run`]; the test
//! `every_table_command_is_mapped` iterates the table, so a new command
//! cannot be dropped silently.

use modron_mobile_core::view::{SLASH_NEEDS_ARG_PREFIX, SLASH_UNKNOWN_LABEL};
use modron_mobile_core::{ApprovalModeWire, Effort, Phase, SandboxPolicy};
use modron_view::slash::{slash_lookup, suggestions, SLASH_COMMANDS};

use crate::chat::UiAction;
use crate::nav::{NavStep, Screen};

/// One command row: what the suggestions and help show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: &'static str,
    pub usage: &'static str,
    pub about: &'static str,
}

/// Commands this app serves beyond the shared table.
pub const LOCAL: &[Entry] = &[
    Entry { name: "sessions", usage: "/sessions", about: "open the sessions list" },
    Entry { name: "compact", usage: "/compact", about: "compact the session's history now" },
    Entry {
        name: "effort",
        usage: "/effort [low|medium|high|xhigh|max|unset]",
        about: "reasoning effort for future turns",
    },
    Entry { name: "plan", usage: "/plan", about: "plan phase for future turns (read-only)" },
    Entry { name: "build", usage: "/build", about: "build phase for future turns" },
];

/// Every command: the shared table in its order, then [`LOCAL`].
pub fn all() -> Vec<Entry> {
    SLASH_COMMANDS
        .iter()
        .map(|c| Entry { name: c.name, usage: c.usage, about: c.about })
        .chain(LOCAL.iter().copied())
        .collect()
}

/// The suggestions for the composer text: none unless the text is a slash
/// word with no arguments yet. Prefix matches, in table order.
pub fn suggestions_for(text: &str) -> Vec<Entry> {
    let Some(rest) = text.strip_prefix('/') else {
        return Vec::new();
    };
    if rest.chars().any(char::is_whitespace) {
        return Vec::new();
    }
    let token = rest.to_lowercase();
    let table = suggestions(&token);
    all()
        .into_iter()
        .filter(|e| {
            if LOCAL.iter().any(|l| l.name == e.name) {
                e.name.starts_with(&token)
            } else {
                table.contains(&e.name)
            }
        })
        .collect()
}

/// True when the text is exactly a command word (Enter then runs it
/// instead of completing the selected suggestion).
pub fn is_exact(text: &str) -> bool {
    let Some(word) = text.strip_prefix('/') else {
        return false;
    };
    let word = word.to_lowercase();
    slash_lookup(&word).is_some() || LOCAL.iter().any(|l| l.name == word)
}

/// The composer text that completes `name`: the word, with a trailing space
/// when the usage takes an argument.
pub fn complete_text(name: &str) -> String {
    let takes_arg = all()
        .iter()
        .find(|e| e.name == name)
        .is_some_and(|e| e.usage.contains('[') || e.usage.contains('<'));
    if takes_arg {
        format!("/{name} ")
    } else {
        format!("/{name}")
    }
}

/// The action a submitted slash line means. `line` starts with `/`.
pub fn run(line: &str) -> UiAction {
    let body = line.trim().strip_prefix('/').unwrap_or(line.trim());
    let (word, args) = match body.split_once(char::is_whitespace) {
        Some((word, args)) => (word, args.trim()),
        None => (body, ""),
    };
    let word = word.to_lowercase();
    let usage = |name: &str| {
        all()
            .iter()
            .find(|e| e.name == name)
            .map(|e| UiAction::Notice(format!("{SLASH_NEEDS_ARG_PREFIX}{}", e.usage)))
            .unwrap_or_else(|| UiAction::Notice(SLASH_UNKNOWN_LABEL.to_string()))
    };
    match word.as_str() {
        "new" => UiAction::NewSession,
        "compact" => UiAction::Compact,
        "sessions" => UiAction::Nav(NavStep::Open(Screen::Sessions)),
        "settings" | "auth" => UiAction::Nav(NavStep::Open(Screen::Settings)),
        "help" => UiAction::Nav(NavStep::Open(Screen::Help)),
        "model" if args.is_empty() => UiAction::Nav(NavStep::Open(Screen::Models)),
        "model" => UiAction::SelectModel(args.to_string()),
        "mode" => match args {
            "" => UiAction::CycleMode,
            "ask" => UiAction::SetApprovalMode(ApprovalModeWire::Ask),
            "yolo" => UiAction::SetApprovalMode(ApprovalModeWire::ApproveAll),
            "deny" => UiAction::SetApprovalMode(ApprovalModeWire::Deny),
            _ => usage("mode"),
        },
        "sandbox" => match args {
            "" => UiAction::CycleSandbox,
            "off" => UiAction::SetSandbox(SandboxPolicy::Off),
            "seatbelt" => UiAction::SetSandbox(SandboxPolicy::Seatbelt),
            "bwrap" => UiAction::SetSandbox(SandboxPolicy::Bubblewrap),
            _ => usage("sandbox"),
        },
        "effort" => match args {
            "low" => UiAction::SetEffort(Some(Effort::Low)),
            "medium" => UiAction::SetEffort(Some(Effort::Medium)),
            "high" => UiAction::SetEffort(Some(Effort::High)),
            "xhigh" => UiAction::SetEffort(Some(Effort::Xhigh)),
            "max" => UiAction::SetEffort(Some(Effort::Max)),
            "unset" => UiAction::SetEffort(None),
            _ => usage("effort"),
        },
        "plan" => UiAction::SetPhase(Phase::Plan),
        "build" => UiAction::SetPhase(Phase::Build),
        "fork" => UiAction::Fork,
        "tasks" => UiAction::TaskStatus,
        "share" => UiAction::Share,
        "remote-control" if args.is_empty() => usage("remote-control"),
        "remote-control" => UiAction::RemoteControl(args.to_string()),
        _ => UiAction::Notice(SLASH_UNKNOWN_LABEL.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_unknown(action: &UiAction) -> bool {
        *action == UiAction::Notice(SLASH_UNKNOWN_LABEL.to_string())
    }

    #[test]
    fn every_table_command_is_mapped() {
        for spec in SLASH_COMMANDS {
            let action = run(&format!("/{}", spec.name));
            assert!(!is_unknown(&action), "table command /{} has no branch", spec.name);
        }
    }

    #[test]
    fn every_local_command_is_mapped() {
        for entry in LOCAL {
            assert!(!is_unknown(&run(&format!("/{}", entry.name))), "/{}", entry.name);
        }
    }

    #[test]
    fn the_listed_commands_are_unique() {
        let mut names: Vec<&str> = all().iter().map(|e| e.name).collect();
        let len = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), len);
    }

    #[test]
    fn unknown_words_get_the_shared_notice() {
        assert!(is_unknown(&run("/nope")));
        assert!(is_unknown(&run("/remote")));
    }

    #[test]
    fn model_bare_opens_the_picker_and_with_a_name_switches() {
        assert_eq!(run("/model"), UiAction::Nav(NavStep::Open(Screen::Models)));
        assert_eq!(
            run("/model claude-haiku-5-5"),
            UiAction::SelectModel("claude-haiku-5-5".into())
        );
    }

    #[test]
    fn local_commands_map_to_their_actions() {
        assert_eq!(run("/compact"), UiAction::Compact);
        assert_eq!(run("/new"), UiAction::NewSession);
        assert_eq!(run("/settings"), UiAction::Nav(NavStep::Open(Screen::Settings)));
        assert_eq!(run("/mode yolo"), UiAction::SetApprovalMode(ApprovalModeWire::ApproveAll));
        assert_eq!(run("/mode"), UiAction::CycleMode);
        assert_eq!(run("/effort max"), UiAction::SetEffort(Some(Effort::Max)));
        assert_eq!(run("/build"), UiAction::SetPhase(Phase::Build));
        assert_eq!(run("/remote-control ws://x"), UiAction::RemoteControl("ws://x".into()));
    }

    #[test]
    fn bad_arguments_show_the_usage_line() {
        assert_eq!(run("/mode maybe"), UiAction::Notice("use /mode [ask|yolo|deny]".into()));
        assert!(matches!(run("/remote-control"), UiAction::Notice(s) if s.starts_with("use ")));
    }

    #[test]
    fn typing_co_suggests_compact_and_completion_keeps_the_word() {
        let names: Vec<&str> = suggestions_for("/co").iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["compact"]);
        assert_eq!(complete_text("model"), "/model ");
        assert_eq!(complete_text("help"), "/help");
    }

    #[test]
    fn arguments_close_the_list_and_exact_words_are_marked() {
        assert!(suggestions_for("/model x").is_empty());
        assert!(suggestions_for("hello").is_empty());
        assert!(is_exact("/help"));
        assert!(!is_exact("/he"));
    }
}
