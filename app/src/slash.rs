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

use std::borrow::Cow;
use std::sync::Mutex;

use modron_mobile_core::view::{
    btw_task, learn_task, plan_task, BTW_USAGE, PROJECT_COMMAND_ABOUT, REVIEW_TASK,
    SLASH_NEEDS_ARG_PREFIX, SLASH_UNKNOWN_LABEL,
};
use modron_mobile_core::{ApprovalModeWire, Effort, Phase, SandboxPolicy};
use modron_view::slash::{slash_lookup, SLASH_COMMANDS};

use crate::chat::{UiAction, WEB_CONTROLS};
use crate::nav::{NavStep, PickerKind, Screen};
use crate::theme::Choice;

/// One command row: what the suggestions and help show. `name` and `usage`
/// are owned for a project command (its name arrives at run time).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: Cow<'static, str>,
    pub usage: Cow<'static, str>,
    pub about: &'static str,
}

/// The project command names the server listed in its last welcome
/// (`.claude/commands`). Set from each snapshot; read by [`run`],
/// [`is_exact`] and the list functions. Empty until a welcome lists some,
/// so a `/word` is then a local notice (the safe default).
static PROJECT_COMMANDS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Record the project command names of the current server (a snapshot's
/// `project_commands`). An empty list clears them.
pub fn set_project_commands(names: &[String]) {
    if let Ok(mut known) = PROJECT_COMMANDS.lock() {
        if known.as_slice() != names {
            *known = names.to_vec();
        }
    }
}

/// The project command names last recorded by [`set_project_commands`].
pub fn project_commands() -> Vec<String> {
    PROJECT_COMMANDS.lock().map(|known| known.clone()).unwrap_or_default()
}

/// One popup row per project command, in the given order. Usage takes the
/// command's arguments (`$ARGUMENTS` in the file), so completion adds a space.
pub fn project_entries(names: &[String]) -> Vec<Entry> {
    names
        .iter()
        .map(|name| Entry {
            name: Cow::Owned(name.clone()),
            usage: Cow::Owned(format!("/{name} [args]")),
            about: PROJECT_COMMAND_ABOUT,
        })
        .collect()
}

/// Commands this app serves beyond the shared table. Empty: `/sessions`,
/// `/compact`, `/effort`, `/plan` and `/build` moved into
/// `modron_view::slash::SLASH_COMMANDS`, so every client lists them.
pub const LOCAL: &[Entry] = &[
    Entry {
        name: Cow::Borrowed("review"),
        usage: Cow::Borrowed("/review"),
        about: "review the working diff",
    },
    Entry {
        name: Cow::Borrowed("btw"),
        usage: Cow::Borrowed("/btw <quick question>"),
        about: "quick side question, no tools",
    },
    Entry {
        name: Cow::Borrowed("learn"),
        usage: Cow::Borrowed("/learn [title]"),
        about: "distill this session into a SKILL.md",
    },
    Entry {
        name: Cow::Borrowed("goal"),
        usage: Cow::Borrowed("/goal [condition]"),
        about: "stop condition for long runs",
    },
    Entry {
        name: Cow::Borrowed("agent"),
        usage: Cow::Borrowed("/agent [name]"),
        about: "switch persona (bare: pick one)",
    },
    Entry { name: Cow::Borrowed("exit"), usage: Cow::Borrowed("/exit"), about: "leave the app" },
    Entry {
        name: Cow::Borrowed("clear"),
        usage: Cow::Borrowed("/clear"),
        about: "clear the transcript view (nothing is sent)",
    },
    Entry {
        name: Cow::Borrowed("context"),
        usage: Cow::Borrowed("/context"),
        about: "context window, tokens and cache use",
    },
    Entry {
        name: Cow::Borrowed("theme"),
        usage: Cow::Borrowed("/theme [name]"),
        about: "pick a theme: oculus dark, oculus light or system",
    },
];

/// The app's own commands for this target: [`LOCAL`], plus `/cloud` on the
/// web (the owner's Cloud screen; the terminal has no bridge to show it).
pub fn local_entries() -> Vec<Entry> {
    let mut entries = LOCAL.to_vec();
    if WEB_CONTROLS {
        entries.push(Entry {
            name: Cow::Borrowed("cloud"),
            usage: Cow::Borrowed("/cloud"),
            about: "cloud runs: github accounts, runs",
        });
    }
    entries
}

/// The shared table as entries, in its order.
fn table_entries() -> Vec<Entry> {
    SLASH_COMMANDS
        .iter()
        .map(|c| Entry {
            name: Cow::Borrowed(c.name),
            usage: Cow::Borrowed(c.usage),
            about: c.about,
        })
        .collect()
}

/// Merge the shared table with the app's own list, de-duplicated by name.
/// The shared entry wins for a name both lists have (its usage and copy are
/// the ones every client shows); the app's entry is dropped. Order: the
/// shared table first, then the app's remaining rows.
pub fn merge(shared: &[Entry], local: &[Entry]) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::with_capacity(shared.len() + local.len());
    for entry in shared.iter().chain(local.iter()) {
        if !out.iter().any(|e| e.name == entry.name) {
            out.push(entry.clone());
        }
    }
    out
}

/// Every command: the shared table in its order, then the app's own rows
/// the table does not name, then the project commands the server listed
/// (after the built-ins). Each name appears once.
pub fn all() -> Vec<Entry> {
    all_with(&project_commands())
}

/// [`all`] for an explicit project command list (the tests use this form).
pub fn all_with(commands: &[String]) -> Vec<Entry> {
    let mut out = merge(&table_entries(), &local_entries());
    for entry in project_entries(commands) {
        if !out.iter().any(|e| e.name == entry.name) {
            out.push(entry);
        }
    }
    out
}

/// The suggestions for the composer text: none unless the text is a slash
/// word with no arguments yet. Prefix matches, in table order.
pub fn suggestions_for(text: &str) -> Vec<Entry> {
    suggestions_with(text, &project_commands())
}

/// [`suggestions_for`] for an explicit project command list.
pub fn suggestions_with(text: &str, commands: &[String]) -> Vec<Entry> {
    let Some(rest) = text.strip_prefix('/') else {
        return Vec::new();
    };
    if rest.chars().any(char::is_whitespace) {
        return Vec::new();
    }
    // Prefix matches over the merged list, in its order: a name shows once.
    let token = rest.to_lowercase();
    all_with(commands).into_iter().filter(|e| e.name.starts_with(&token)).collect()
}

/// True when the text is exactly a command word (Enter then runs it
/// instead of completing the selected suggestion).
pub fn is_exact(text: &str) -> bool {
    is_exact_with(text, &project_commands())
}

/// [`is_exact`] for an explicit project command list (a project command's
/// name is matched as written: file names are case-sensitive).
pub fn is_exact_with(text: &str, commands: &[String]) -> bool {
    let Some(word) = text.strip_prefix('/') else {
        return false;
    };
    let is_project = commands.iter().any(|name| name == word);
    let word = word.to_lowercase();
    slash_lookup(&word).is_some() || local_entries().iter().any(|l| l.name == word) || is_project
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
    run_with(line, &project_commands())
}

/// [`run`] for an explicit project command list. A built-in word always wins;
/// a listed project command is sent as the task (the engine expands it); any
/// other word is the local unknown-command notice, not a model turn.
pub fn run_with(line: &str, commands: &[String]) -> UiAction {
    let body = line.trim().strip_prefix('/').unwrap_or(line.trim());
    let (word, args) = match body.split_once(char::is_whitespace) {
        Some((word, args)) => (word, args.trim()),
        None => (body, ""),
    };
    // Project command names are case-sensitive file names: matched as written.
    let raw_word = word.to_string();
    // The shared table resolves first (a shared alias or renamed entry maps
    // to its own name); the arms below are the app's dispatch, including the
    // aliases the table does not carry.
    let word = word.to_lowercase();
    let word = slash_lookup(&word).map(|spec| spec.name.to_string()).unwrap_or(word);
    let usage = |name: &str| {
        all_with(commands)
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
                // `modron auth login --provider chatgpt` names the row the
                // server lists as `openai-compat` ("OpenAI / ChatGPT").
                let id = if provider == "chatgpt" { "openai-compat" } else { provider };
                UiAction::Nav(NavStep::AuthFor(id.to_string()))
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
        // Team mode runs on the server: the line goes out as the task and the
        // engine reads the `/team [N] <task>` form (a bare `/team` is usage).
        "team" if args.is_empty() => usage("team"),
        "team" => UiAction::Send(format!("/team {args}")),
        // `/fork` forks the tip; `/fork <n>` forks after user turn n. The
        // number is sent as is: the server refuses 0 and out-of-range turns.
        "fork" if args.is_empty() => UiAction::Fork(None),
        "fork" => match args.parse::<u32>() {
            Ok(turn) => UiAction::Fork(Some(turn)),
            Err(_) => usage("fork"),
        },
        "tasks" => UiAction::Nav(NavStep::Open(Screen::Tasks)),
        "cloud" if WEB_CONTROLS => UiAction::Nav(NavStep::Open(Screen::Cloud)),
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
        // Not a built-in. A project command the server listed is a task (the
        // engine expands `.claude/commands/<word>.md`); any other word is a
        // typo, so it gets the local notice rather than a model turn.
        _ if commands.contains(&raw_word) => UiAction::Send(line.trim().to_string()),
        _ => UiAction::Notice(SLASH_UNKNOWN_LABEL.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_unknown(action: &UiAction) -> bool {
        *action == UiAction::Notice(SLASH_UNKNOWN_LABEL.to_string())
    }

    /// The line a non-built-in `/word` sends as a task.
    fn sent(line: &str) -> UiAction {
        UiAction::Send(line.to_string())
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
        for entry in local_entries() {
            assert!(!is_unknown(&run(&format!("/{}", entry.name))), "/{}", entry.name);
        }
    }

    #[test]
    fn auth_chatgpt_names_the_openai_compat_row() {
        assert_eq!(run("/auth chatgpt"), UiAction::Nav(NavStep::AuthFor("openai-compat".into())));
        assert_eq!(run("/auth anthropic"), UiAction::Nav(NavStep::AuthFor("anthropic".into())));
    }

    #[test]
    fn fork_takes_an_optional_turn() {
        assert_eq!(run("/fork"), UiAction::Fork(None));
        assert_eq!(run("/fork 2"), UiAction::Fork(Some(2)));
        // Zero and out-of-range turns are the server's to refuse, so they go out as numbers.
        assert_eq!(run("/fork 0"), UiAction::Fork(Some(0)));
        // Not a number: the usage line, nothing sent.
        assert!(matches!(run("/fork two"), UiAction::Notice(_)));
    }

    #[test]
    fn cloud_is_a_web_only_command() {
        // The native tests are the terminal target: no /cloud there, so it is
        // not a built-in and, not being a listed project command, is a local
        // notice like any unknown word (never a model turn).
        assert!(is_unknown(&run_with("/cloud", &[])));
        assert!(!local_entries().iter().any(|e| e.name == "cloud"));
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_listed_project_command_is_sent_and_an_unlisted_word_is_a_notice() {
        let known = names(&["deploy", "triage"]);
        assert_eq!(run_with("/deploy staging", &known), sent("/deploy staging"));
        assert_eq!(run_with("  /triage ", &known), sent("/triage"));
        // A typo reaches no model: it is the local notice.
        assert!(is_unknown(&run_with("/depoly staging", &known)));
        // Project command names are case-sensitive file names.
        assert!(is_unknown(&run_with("/Deploy", &known)));
    }

    #[test]
    fn an_old_server_lists_nothing_so_every_unknown_word_is_a_notice() {
        // A welcome without `commands` reads as an empty list.
        assert!(is_unknown(&run_with("/deploy", &[])));
        assert!(!is_exact_with("/deploy", &[]));
    }

    #[test]
    fn a_built_in_word_wins_over_a_project_command_of_the_same_name() {
        let known = names(&["help"]);
        assert_eq!(run_with("/help", &known), run("/help"));
        assert!(!matches!(run_with("/help", &known), UiAction::Send(_)));
    }

    #[test]
    fn project_commands_list_after_the_built_ins_and_complete_with_args() {
        let known = names(&["deploy"]);
        let all = all_with(&known);
        assert_eq!(all.last().map(|e| e.name.to_string()), Some("deploy".to_string()));
        let deploy = all.last().unwrap();
        assert_eq!(deploy.usage, "/deploy [args]");
        assert_eq!(deploy.about, PROJECT_COMMAND_ABOUT);
        // The popup filters the merged list by prefix, project rows included.
        let rows: Vec<String> =
            suggestions_with("/dep", &known).iter().map(|e| e.name.to_string()).collect();
        assert_eq!(rows, vec!["deploy"]);
        // Exact match: Enter runs it instead of completing.
        assert!(is_exact_with("/deploy", &known));
        assert!(!is_exact_with("/depl", &known));
    }

    #[test]
    fn the_recorded_list_is_set_and_cleared_as_one_snapshot_changes() {
        set_project_commands(&names(&["deploy"]));
        assert_eq!(project_commands(), names(&["deploy"]));
        set_project_commands(&[]);
        assert!(project_commands().is_empty());
    }

    #[test]
    fn the_listed_commands_are_unique() {
        let mut names: Vec<String> = all().iter().map(|e| e.name.to_string()).collect();
        let len = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), len);
    }

    #[test]
    fn a_shared_entry_wins_over_a_local_one_with_the_same_name() {
        let shared = [Entry {
            name: Cow::Borrowed("review"),
            usage: Cow::Borrowed("/review [focus]"),
            about: "shared",
        }];
        let local = [
            Entry {
                name: Cow::Borrowed("review"),
                usage: Cow::Borrowed("/review"),
                about: "local",
            },
            Entry { name: Cow::Borrowed("exit"), usage: Cow::Borrowed("/exit"), about: "leave" },
        ];
        let merged = merge(&shared, &local);
        let names: Vec<&str> = merged.iter().map(|e| e.name.as_ref()).collect();
        assert_eq!(names, vec!["review", "exit"]);
        assert_eq!(merged[0].about, "shared");
    }

    #[test]
    fn suggestions_never_show_a_name_twice() {
        let names: Vec<String> = suggestions_for("/").iter().map(|e| e.name.to_string()).collect();
        let mut unique = names.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(names.len(), unique.len());
    }

    #[test]
    fn unknown_words_are_local_notices_unless_listed() {
        // Only a listed project command (`.claude/commands/<word>.md`) is sent;
        // the engine expands it at submit time. Any other word is a typo and
        // must not reach the model.
        let known = names(&["fix-bug"]);
        assert!(is_unknown(&run_with("/nope", &known)));
        assert_eq!(run_with("/fix-bug 123 now", &known), sent("/fix-bug 123 now"));
        assert!(is_unknown(&run_with("/fix-bugs 123", &known)));
        assert!(is_unknown(&run_with("/nope", &[])));
    }

    #[test]
    fn team_is_a_server_task_and_bare_team_shows_usage() {
        assert_eq!(run("/team 4 refactor the parser"), sent("/team 4 refactor the parser"));
        assert_eq!(run("/team fix it"), sent("/team fix it"));
        assert!(matches!(run("/team"), UiAction::Notice(s) if s.starts_with("use ")));
        assert!(is_exact("/team"));
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
        let names: Vec<String> =
            suggestions_for("/co").iter().map(|e| e.name.to_string()).collect();
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
