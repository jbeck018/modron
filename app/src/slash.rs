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

use modron_mobile_core::view::{
    btw_task, learn_task, plan_task, BTW_USAGE, REVIEW_TASK, SLASH_NEEDS_ARG_PREFIX,
    SLASH_UNKNOWN_LABEL,
};
use modron_mobile_core::{ApprovalModeWire, Effort, Phase, SandboxPolicy};
use modron_view::slash::{slash_lookup, SLASH_COMMANDS};

use crate::chat::UiAction;
use crate::nav::{NavStep, PickerKind, Screen};
use crate::theme::Choice;

/// One command row: what the suggestions and help show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: &'static str,
    pub usage: &'static str,
    pub about: &'static str,
}

/// Commands this app serves beyond the shared table. Empty: `/sessions`,
/// `/compact`, `/effort`, `/plan` and `/build` moved into
/// `modron_view::slash::SLASH_COMMANDS`, so every client lists them.
pub const LOCAL: &[Entry] = &[
    Entry { name: "review", usage: "/review", about: "review the working diff" },
    Entry { name: "btw", usage: "/btw <quick question>", about: "quick side question, no tools" },
    Entry { name: "learn", usage: "/learn [title]", about: "distill this session into a SKILL.md" },
    Entry { name: "goal", usage: "/goal [condition]", about: "stop condition for long runs" },
    Entry { name: "agent", usage: "/agent [name]", about: "switch persona (bare: pick one)" },
    Entry { name: "exit", usage: "/exit", about: "leave the app" },
    Entry { name: "clear", usage: "/clear", about: "clear the transcript view (nothing is sent)" },
    Entry { name: "context", usage: "/context", about: "context window, tokens and cache use" },
    Entry {
        name: "theme",
        usage: "/theme [name]",
        about: "pick a theme: oculus dark, oculus light or system",
    },
];

/// The shared table as entries, in its order.
fn table_entries() -> Vec<Entry> {
    SLASH_COMMANDS.iter().map(|c| Entry { name: c.name, usage: c.usage, about: c.about }).collect()
}

/// Merge the shared table with the app's own list, de-duplicated by name.
/// The shared entry wins for a name both lists have (its usage and copy are
/// the ones every client shows); the app's entry is dropped. Order: the
/// shared table first, then the app's remaining rows.
pub fn merge(shared: &[Entry], local: &[Entry]) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::with_capacity(shared.len() + local.len());
    for entry in shared.iter().chain(local.iter()) {
        if !out.iter().any(|e| e.name == entry.name) {
            out.push(*entry);
        }
    }
    out
}

/// Every command: the shared table in its order, then the app's own rows
/// the table does not name. Each name appears once.
pub fn all() -> Vec<Entry> {
    merge(&table_entries(), LOCAL)
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
    // Prefix matches over the merged list, in its order: a name shows once.
    let token = rest.to_lowercase();
    all().into_iter().filter(|e| e.name.starts_with(&token)).collect()
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
    // The shared table resolves first (a shared alias or renamed entry maps
    // to its own name); the arms below are the app's dispatch, including the
    // aliases the table does not carry.
    let word = word.to_lowercase();
    let word = slash_lookup(&word).map(|spec| spec.name.to_string()).unwrap_or(word);
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
        "settings" => UiAction::Nav(NavStep::Open(Screen::Settings)),
        "login" => UiAction::Nav(NavStep::Open(Screen::Auth)),
        "compress" => UiAction::Compact,
        "auth" => match args {
            "" | "add" | "manage" => UiAction::Nav(NavStep::Open(Screen::Auth)),
            // `/auth <provider> ...`: the first word names the provider (the
            // old terminal's `<provider> <key>` form pre-fills nothing here).
            rest => {
                let provider = rest.split_whitespace().next().unwrap_or_default();
                UiAction::Nav(NavStep::AuthFor(provider.to_string()))
            }
        },
        "help" => UiAction::Nav(NavStep::Open(Screen::Help)),
        "model" if args.is_empty() => UiAction::Nav(NavStep::Open(Screen::Models)),
        "model" => UiAction::SelectModel(args.to_string()),
        "mode" => match args {
            "" => UiAction::Nav(NavStep::OpenPicker(PickerKind::Mode)),
            "ask" => UiAction::SetApprovalMode(ApprovalModeWire::Ask),
            "yolo" => UiAction::SetApprovalMode(ApprovalModeWire::ApproveAll),
            "deny" => UiAction::SetApprovalMode(ApprovalModeWire::Deny),
            _ => usage("mode"),
        },
        "sandbox" => match args {
            "" => UiAction::Nav(NavStep::OpenPicker(PickerKind::Sandbox)),
            "off" => UiAction::SetSandbox(SandboxPolicy::Off),
            "seatbelt" => UiAction::SetSandbox(SandboxPolicy::Seatbelt),
            "bwrap" | "bubblewrap" => UiAction::SetSandbox(SandboxPolicy::Bubblewrap),
            _ => usage("sandbox"),
        },
        "effort" => match args {
            "" => UiAction::Nav(NavStep::OpenPicker(PickerKind::Effort)),
            "low" => UiAction::SetEffort(Some(Effort::Low)),
            "medium" => UiAction::SetEffort(Some(Effort::Medium)),
            "high" => UiAction::SetEffort(Some(Effort::High)),
            "xhigh" => UiAction::SetEffort(Some(Effort::Xhigh)),
            "max" => UiAction::SetEffort(Some(Effort::Max)),
            "off" | "unset" => UiAction::SetEffort(None),
            _ => usage("effort"),
        },
        "reasoning" | "thinking" => UiAction::ToggleReasoning,
        "plan" if args.is_empty() => UiAction::SetPhase(Phase::Plan),
        "plan" => UiAction::Plan(plan_task(args)),
        "review" | "code-review" => UiAction::Send(REVIEW_TASK.to_string()),
        "btw" if args.is_empty() => UiAction::Notice(BTW_USAGE.to_string()),
        "btw" => UiAction::Send(btw_task(args)),
        "learn" => UiAction::Send(learn_task(args)),
        "goal" => UiAction::SetGoal(args.to_string()),
        "agent" | "persona" if args.is_empty() => {
            UiAction::Nav(NavStep::OpenPicker(PickerKind::Agent))
        }
        "agent" | "persona" => UiAction::SetPersona(args.to_string()),
        "exit" | "quit" | "q" => UiAction::Exit,
        "clear" => UiAction::Clear,
        "context" => UiAction::Context,
        "theme" => match args {
            "" => UiAction::Nav(NavStep::OpenPicker(PickerKind::Theme)),
            name => match Choice::parse(name) {
                Some(choice) => UiAction::Nav(NavStep::SetTheme(choice)),
                None => usage("theme"),
            },
        },
        "build" => UiAction::SetPhase(Phase::Build),
        "fork" => UiAction::Fork,
        "tasks" => UiAction::Nav(NavStep::Open(Screen::Tasks)),
        "allow" | "deny" if args.is_empty() => {
            UiAction::Nav(NavStep::OpenToolList(word == "allow"))
        }
        "allow" => UiAction::AddToolRule { allow: true, tool: args.to_string() },
        "deny" => UiAction::AddToolRule { allow: false, tool: args.to_string() },
        "automations" => UiAction::Nav(NavStep::Open(Screen::Automations)),
        // Bare `/worktree` opens the tasks screen; with a title it creates the task.
        "worktree" | "worktrees" if args.is_empty() => UiAction::Nav(NavStep::Open(Screen::Tasks)),
        "worktree" | "worktrees" => UiAction::CreateTask(args.to_string()),
        "share" | "publish" => UiAction::Share,
        "remote-control" | "rc" if args.is_empty() => usage("remote-control"),
        "remote-control" | "rc" => UiAction::RemoteControl(args.to_string()),
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
    fn a_shared_entry_wins_over_a_local_one_with_the_same_name() {
        let shared = [Entry { name: "review", usage: "/review [focus]", about: "shared" }];
        let local = [
            Entry { name: "review", usage: "/review", about: "local" },
            Entry { name: "exit", usage: "/exit", about: "leave" },
        ];
        let merged = merge(&shared, &local);
        let names: Vec<&str> = merged.iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["review", "exit"]);
        assert_eq!(merged[0].about, "shared");
    }

    #[test]
    fn suggestions_never_show_a_name_twice() {
        let names: Vec<&str> = suggestions_for("/").iter().map(|e| e.name).collect();
        let mut unique = names.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(names.len(), unique.len());
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
    fn theme_bare_opens_the_picker_and_a_name_applies_it() {
        assert_eq!(run("/theme"), UiAction::Nav(NavStep::OpenPicker(PickerKind::Theme)));
        assert_eq!(run("/theme dark"), UiAction::Nav(NavStep::SetTheme(Choice::Dark)));
        assert_eq!(run("/theme Oculus Light"), UiAction::Nav(NavStep::SetTheme(Choice::Light)));
        assert_eq!(run("/theme system"), UiAction::Nav(NavStep::SetTheme(Choice::System)));
        assert!(matches!(run("/theme dracula"), UiAction::Notice(_)));
    }

    #[test]
    fn reasoning_and_thinking_both_toggle_the_reasoning_tail() {
        assert_eq!(run("/reasoning"), UiAction::ToggleReasoning);
        assert_eq!(run("/thinking"), UiAction::ToggleReasoning);
    }

    #[test]
    fn the_picker_commands_open_their_pickers_and_their_arguments_apply_directly() {
        assert_eq!(run("/mode"), UiAction::Nav(NavStep::OpenPicker(PickerKind::Mode)));
        assert_eq!(run("/sandbox"), UiAction::Nav(NavStep::OpenPicker(PickerKind::Sandbox)));
        assert_eq!(run("/effort"), UiAction::Nav(NavStep::OpenPicker(PickerKind::Effort)));
        assert_eq!(run("/agent"), UiAction::Nav(NavStep::OpenPicker(PickerKind::Agent)));
        assert_eq!(run("/mode yolo"), UiAction::SetApprovalMode(ApprovalModeWire::ApproveAll));
        assert_eq!(run("/effort high"), UiAction::SetEffort(Some(Effort::High)));
        assert_eq!(run("/agent reviewer"), UiAction::SetPersona("reviewer".to_string()));
    }

    #[test]
    fn local_commands_map_to_their_actions() {
        assert_eq!(run("/compact"), UiAction::Compact);
        assert_eq!(run("/new"), UiAction::NewSession);
        assert_eq!(run("/settings"), UiAction::Nav(NavStep::Open(Screen::Settings)));
        assert_eq!(run("/mode yolo"), UiAction::SetApprovalMode(ApprovalModeWire::ApproveAll));
        assert_eq!(run("/effort max"), UiAction::SetEffort(Some(Effort::Max)));
        assert_eq!(run("/build"), UiAction::SetPhase(Phase::Build));
        assert_eq!(run("/exit"), UiAction::Exit);
        assert_eq!(run("/clear"), UiAction::Clear);
        assert_eq!(run("/q"), UiAction::Exit);
        assert_eq!(run("/login"), UiAction::Nav(NavStep::Open(Screen::Auth)));
        assert_eq!(run("/auth add"), UiAction::Nav(NavStep::Open(Screen::Auth)));
        assert_eq!(run("/auth openai"), UiAction::Nav(NavStep::AuthFor("openai".into())));
        assert_eq!(run("/allow"), UiAction::Nav(NavStep::OpenToolList(true)));
        assert_eq!(run("/deny"), UiAction::Nav(NavStep::OpenToolList(false)));
        assert_eq!(run("/sandbox bubblewrap"), UiAction::SetSandbox(SandboxPolicy::Bubblewrap));
        assert_eq!(run("/sandbox bwrap"), UiAction::SetSandbox(SandboxPolicy::Bubblewrap));
        assert_eq!(run("/rc ws://x"), UiAction::RemoteControl("ws://x".into()));
        assert_eq!(run("/goal ship it"), UiAction::SetGoal("ship it".into()));
        assert_eq!(run("/goal"), UiAction::SetGoal(String::new()));
        assert_eq!(run("/agent reviewer"), UiAction::SetPersona("reviewer".into()));
        assert_eq!(run("/review"), UiAction::Send(REVIEW_TASK.into()));
        assert_eq!(run("/plan add tests"), UiAction::Plan(plan_task("add tests")));
        assert_eq!(run("/plan"), UiAction::SetPhase(Phase::Plan));
        assert_eq!(run("/btw why"), UiAction::Send(btw_task("why")));
        assert_eq!(run("/btw"), UiAction::Notice(BTW_USAGE.into()));
        assert_eq!(run("/learn Fix Parser"), UiAction::Send(learn_task("Fix Parser")));
        assert_eq!(run("/remote-control ws://x"), UiAction::RemoteControl("ws://x".into()));
        assert_eq!(run("/automations"), UiAction::Nav(NavStep::Open(Screen::Automations)));
        assert_eq!(run("/tasks"), UiAction::Nav(NavStep::Open(Screen::Tasks)));
        assert_eq!(run("/worktree"), UiAction::Nav(NavStep::Open(Screen::Tasks)));
        assert_eq!(run("/worktree fix parser"), UiAction::CreateTask("fix parser".into()));
    }

    #[test]
    fn bad_arguments_show_the_usage_line() {
        assert_eq!(run("/mode maybe"), UiAction::Notice("use /mode [ask|yolo|deny]".into()));
        assert!(matches!(run("/remote-control"), UiAction::Notice(s) if s.starts_with("use ")));
    }

    #[test]
    fn typing_co_suggests_compact_and_completion_keeps_the_word() {
        let names: Vec<&str> = suggestions_for("/co").iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["compact", "context"]);
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
