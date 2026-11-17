//! The shared slash-command table (2026-11-11): one canonical list the
//! TUI, desktop GUI, and the web/mobile shell all serve. The names and
//! wordings are the contract (AGENTS.md parity): each client implements
//! what its surface can do and renders `/help` from the same strings,
//! so a command typed on any client means the same thing.
//!
//! The rows follow the old TUI's `SlashCommand::CATALOG`
//! (crates/modron-tui/src/lib.rs). Its "alias of" rows are carried in
//! [`SlashSpec::aliases`]; the TUI-only words are listed, with reasons, in
//! the test `every_tui_catalog_command_is_shared_or_tui_only`.
//!
//! The canned task text that `/review`, `/btw`, `/plan <task>` and `/learn`
//! submit also lives here, so every client sends the same words.

/// One slash command.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct SlashSpec {
    /// The word after `/` (no slash, no args). The canonical name.
    pub name: &'static str,
    /// Usage line, e.g. `/mode [ask|yolo|deny]`.
    pub usage: &'static str,
    /// One-line description (the shared copy clients render verbatim).
    pub about: &'static str,
    /// Other words that run this command (the old TUI's "alias of" rows),
    /// e.g. `exit` has `quit` and `q`. Empty for most commands.
    pub aliases: &'static [&'static str],
    /// What carries the effect.
    pub scope: Scope,
}

/// Where a command's effect lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Mutates the session over the wire (config, fork, tasks).
    Session,
    /// A one-shot server request (publish a page, open a tunnel).
    Server,
    /// Pure client UI (open settings, show help).
    Client,
}

pub const SLASH_COMMANDS: &[SlashSpec] = &[
    SlashSpec {
        name: "model",
        usage: "/model [name]",
        about: "switch the model for this session; bare opens the picker",
        aliases: &[],
        scope: Scope::Session,
    },
    SlashSpec {
        name: "mode",
        usage: "/mode [ask|yolo|deny]",
        about: "approval gate for tool runs (default ask)",
        aliases: &[],
        scope: Scope::Session,
    },
    SlashSpec {
        name: "sandbox",
        usage: "/sandbox [off|seatbelt|bwrap]",
        about: "filesystem sandbox policy for future turns",
        aliases: &[],
        scope: Scope::Session,
    },
    SlashSpec {
        name: "new",
        usage: "/new",
        about: "start a fresh chat",
        aliases: &[],
        scope: Scope::Session,
    },
    SlashSpec {
        name: "sessions",
        usage: "/sessions",
        about: "open the session list",
        aliases: &[],
        scope: Scope::Client,
    },
    SlashSpec {
        name: "fork",
        usage: "/fork",
        about: "branch the session log; the original is untouched",
        aliases: &[],
        scope: Scope::Session,
    },
    SlashSpec {
        name: "tasks",
        usage: "/tasks",
        about: "status of background delegates and worktree tasks",
        aliases: &[],
        scope: Scope::Session,
    },
    SlashSpec {
        name: "worktree",
        usage: "/worktree",
        about: "list delegate worktrees",
        aliases: &["worktrees"],
        scope: Scope::Session,
    },
    SlashSpec {
        name: "compact",
        usage: "/compact",
        about: "compact this session's context now",
        aliases: &["compress"],
        scope: Scope::Session,
    },
    SlashSpec {
        name: "effort",
        usage: "/effort <low|medium|high|xhigh|max|off>",
        about: "reasoning effort for future turns; off uses the provider default",
        aliases: &[],
        scope: Scope::Session,
    },
    SlashSpec {
        name: "reasoning",
        usage: "/reasoning",
        about: "toggle streamed-reasoning display",
        aliases: &["thinking"],
        scope: Scope::Client,
    },
    SlashSpec {
        name: "goal",
        usage: "/goal [condition]",
        about: "stop condition for long runs",
        aliases: &[],
        scope: Scope::Session,
    },
    SlashSpec {
        name: "plan",
        usage: "/plan [task]",
        about: "switch to the plan phase (read-only); with a task, plan it now",
        aliases: &[],
        scope: Scope::Session,
    },
    SlashSpec {
        name: "build",
        usage: "/build",
        about: "switch to the build phase (edits allowed) for future runs",
        aliases: &[],
        scope: Scope::Session,
    },
    SlashSpec {
        name: "team",
        usage: "/team [N] <task>",
        about: "split the task across N parallel workers (default 3, max 5), merge, verify, fix",
        aliases: &[],
        scope: Scope::Session,
    },
    SlashSpec {
        name: "btw",
        usage: "/btw <quick question>",
        about: "quick side question, no tools",
        aliases: &[],
        scope: Scope::Session,
    },
    SlashSpec {
        name: "review",
        usage: "/review",
        about: "review the working diff",
        aliases: &["code-review"],
        scope: Scope::Session,
    },
    SlashSpec {
        name: "learn",
        usage: "/learn [title]",
        about: "distill this session into a SKILL.md",
        aliases: &[],
        scope: Scope::Session,
    },
    SlashSpec {
        name: "agent",
        usage: "/agent [name]",
        about: "switch persona (picker)",
        aliases: &["persona"],
        scope: Scope::Session,
    },
    SlashSpec {
        name: "allow",
        usage: "/allow [tool]",
        about: "allow a tool (picker)",
        aliases: &[],
        scope: Scope::Session,
    },
    SlashSpec {
        name: "deny",
        usage: "/deny [tool]",
        about: "deny a tool (picker)",
        aliases: &[],
        scope: Scope::Session,
    },
    SlashSpec {
        name: "share",
        usage: "/share",
        about: "publish the transcript as a read-only web page",
        aliases: &["publish"],
        scope: Scope::Server,
    },
    SlashSpec {
        name: "remote-control",
        usage: "/remote-control [bridge-url]",
        about: "open a live tunnel other devices can watch and drive",
        aliases: &["rc"],
        scope: Scope::Server,
    },
    SlashSpec {
        name: "automations",
        usage: "/automations",
        about: "list scheduled automations",
        aliases: &[],
        scope: Scope::Server,
    },
    SlashSpec {
        name: "auth",
        usage: "/auth",
        about: "provider credentials and sign-in status",
        aliases: &["login"],
        scope: Scope::Client,
    },
    SlashSpec {
        name: "settings",
        usage: "/settings",
        about: "open the settings view",
        aliases: &[],
        scope: Scope::Client,
    },
    SlashSpec {
        name: "help",
        usage: "/help",
        about: "list the slash commands",
        aliases: &["?"],
        scope: Scope::Client,
    },
    SlashSpec {
        name: "exit",
        usage: "/exit",
        about: "quit this client (the session keeps running on the server)",
        aliases: &["quit", "q"],
        scope: Scope::Client,
    },
];

/// Slash names only, canonical names only (for parsers). Aliases are
/// reached through [`slash_lookup`].
pub fn slash_names() -> Vec<&'static str> {
    SLASH_COMMANDS.iter().map(|c| c.name).collect()
}

/// The `/help` body, as `usage - about` lines (shared copy).
pub fn slash_help_lines() -> Vec<String> {
    SLASH_COMMANDS
        .iter()
        .map(|c| format!("{} - {}", c.usage, c.about))
        .collect()
}

/// Look up a command by the word after the slash, canonical name or alias.
pub fn slash_lookup(name: &str) -> Option<&'static SlashSpec> {
    SLASH_COMMANDS
        .iter()
        .find(|c| c.name == name || c.aliases.contains(&name))
}

/// Completion candidates for a typed `/token` (prefix match against the
/// name or any alias - the desktop's original semantics; canonical table
/// order). Each command appears once, under its canonical name, even when
/// both the name and an alias match the prefix.
pub fn suggestions(token: &str) -> Vec<&'static str> {
    let token = token.trim_start_matches('/').to_lowercase();
    SLASH_COMMANDS
        .iter()
        .filter(|c| c.name.starts_with(&token) || c.aliases.iter().any(|a| a.starts_with(&token)))
        .map(|c| c.name)
        .collect()
}

// ---------------------------------------------------------------------------
// Canned task text. One home for the words the old TUI submitted
// (crates/modron-tui/src/lib.rs, `SlashCommand::{CodeReview, Btw, Plan,
// Learn}` arms, roughly lines 2570-2715). The old TUI's em dashes are
// written as `\u{2014}` so this source stays plain ASCII; the strings are
// byte-identical to the TUI's.
// ---------------------------------------------------------------------------

/// `/review`: the review task (findings only, nothing fixed).
pub const REVIEW_TASK: &str = "Review the working tree: run `git diff` (and `git status`), then report findings only \u{2014} bugs, risks, and gaps, each with file:line references. Do not fix anything.";
/// The suffix `/btw` appends to the question.
pub const BTW_SUFFIX: &str =
    "(Quick side question: answer directly from what you already know \u{2014} do not use tools.)";
/// The suffix `/plan <task>` appends to the task.
pub const PLAN_SUFFIX: &str = "(Plan first: produce a concrete, step-by-step plan with file references. Do NOT edit any files yet \u{2014} wait for approval.)";

/// `/btw <question>`: the question with the no-tools suffix.
pub fn btw_task(question: &str) -> String {
    format!("{question}\n\n{BTW_SUFFIX}")
}

/// `/plan <task>`: the task with the plan-first suffix.
pub fn plan_task(task: &str) -> String {
    format!("{task}\n\n{PLAN_SUFFIX}")
}

/// The skill-name slug `/learn` uses: ASCII letters and digits lowercased,
/// runs of anything else collapsed to one dash, trimmed, at most 48 chars.
pub fn skill_slug(text: &str) -> String {
    let mut out = String::new();
    for ch in text.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').chars().take(48).collect()
}

/// `/learn [title]`: distil the session into a skill file (the old TUI's
/// wording; an empty title names it `session-skill`).
pub fn learn_task(title: &str) -> String {
    let slug = skill_slug(if title.is_empty() {
        "session-skill"
    } else {
        title
    });
    format!(
        "Distill this session into a reusable skill. Write the file .modron/skills/{slug}/SKILL.md with `---` frontmatter (name: {slug}, description: one line) followed by a concise, step-by-step workflow capturing what actually worked here (commands, file paths, pitfalls). Create the directories. Then reply with just the path you wrote."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every command name the old TUI's `SlashCommand::CATALOG` lists, with
    /// aliases, as literal copies (crates/modron-tui/src/lib.rs lines
    /// 184-219). modron-view cannot import the frozen TUI crate.
    const TUI_CATALOG: &[&str] = &[
        "help",
        "?",
        "exit",
        "quit",
        "q",
        "clear",
        "compact",
        "compress",
        "model",
        "context",
        "sessions",
        "sandbox",
        "reasoning",
        "thinking",
        "btw",
        "agent",
        "persona",
        "allow",
        "deny",
        "mode",
        "tasks",
        "worktree",
        "worktrees",
        "issues",
        "fork",
        "plan",
        "review",
        "code-review",
        "goal",
        "learn",
        "theme",
        "auth",
        "login",
        "settings",
        "new",
        "share",
        "publish",
        "remote-control",
        "rc",
        "automations",
    ];

    /// The TUI's alias rows ("alias of /X"), as literal copies of the same
    /// CATALOG lines: (alias, canonical name).
    const TUI_ALIASES: &[(&str, &str)] = &[
        ("?", "help"),
        ("quit", "exit"),
        ("q", "exit"),
        ("compress", "compact"),
        ("thinking", "reasoning"),
        ("persona", "agent"),
        ("worktrees", "worktree"),
        ("code-review", "review"),
        ("login", "auth"),
        ("publish", "share"),
        ("rc", "remote-control"),
    ];

    /// Words the old TUI serves only in-process. Each needs a reason; the
    /// test checks none of them is in the shared table.
    const TUI_ONLY: &[(&str, &str)] = &[
        (
            "clear",
            "clears the terminal scrollback; a view action of the alt-screen TUI only",
        ),
        (
            "context",
            "prints the local token total; every client shows cumulative tokens in its status surface (invariant 4), so no command is needed",
        ),
        (
            "theme",
            "switches ratatui palette themes; desktop and mobile follow system appearance (AGENTS.md divergence table, Theming)",
        ),
        (
            "issues",
            "reads the TUI's project issue store through its run loop; no protocol frame or other client serves it yet",
        ),
    ];

    #[test]
    fn table_is_sorted_and_unique() {
        let mut names: Vec<&str> = slash_names();
        let len = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), len, "duplicate slash names");
    }

    #[test]
    fn lookup_and_help_agree() {
        let model = slash_lookup("model").expect("model command");
        assert!(model.usage.contains("/model"));
        let lines = slash_help_lines();
        assert_eq!(lines.len(), SLASH_COMMANDS.len());
        assert!(lines.iter().any(|l| l.contains("picker")));
    }

    #[test]
    fn no_two_commands_share_a_name() {
        let mut names: Vec<&str> = SLASH_COMMANDS.iter().map(|c| c.name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "two commands share a name");
        // Every usage line leads with its own name, so `/help` never lists
        // one command under another's word.
        for spec in SLASH_COMMANDS {
            assert!(
                spec.usage.starts_with(&format!("/{}", spec.name)),
                "usage {:?} does not lead with /{}",
                spec.usage,
                spec.name
            );
        }
    }

    #[test]
    fn no_name_or_alias_is_shared_between_commands() {
        let mut words: Vec<&str> = Vec::new();
        for spec in SLASH_COMMANDS {
            words.push(spec.name);
            words.extend_from_slice(spec.aliases);
        }
        let total = words.len();
        words.sort_unstable();
        words.dedup();
        assert_eq!(words.len(), total, "a name or alias is used twice");
        for spec in SLASH_COMMANDS {
            for alias in spec.aliases {
                assert!(
                    !SLASH_COMMANDS.iter().any(|c| c.name == *alias),
                    "alias {alias:?} equals a command name"
                );
            }
        }
    }

    #[test]
    fn every_alias_resolves_to_its_command() {
        for spec in SLASH_COMMANDS {
            assert_eq!(slash_lookup(spec.name), Some(spec), "name /{}", spec.name);
            for alias in spec.aliases {
                assert_eq!(slash_lookup(alias), Some(spec), "alias /{alias}");
            }
        }
        assert!(slash_lookup("nope").is_none());
    }

    #[test]
    fn aliases_match_the_old_tui_alias_rows() {
        for (alias, canonical) in TUI_ALIASES {
            let spec = slash_lookup(alias).unwrap_or_else(|| panic!("/{alias} missing"));
            assert_eq!(spec.name, *canonical, "/{alias} should run /{canonical}");
        }
    }

    #[test]
    fn every_tui_catalog_command_is_shared_or_tui_only() {
        for word in TUI_CATALOG {
            let shared = slash_lookup(word).is_some();
            let tui_only = TUI_ONLY.iter().any(|(name, _)| name == word);
            assert!(
                shared || tui_only,
                "/{word} is in the TUI CATALOG but neither shared nor TUI_ONLY"
            );
        }
        for (name, reason) in TUI_ONLY {
            assert!(!reason.is_empty(), "/{name} has no reason");
            assert!(
                slash_lookup(name).is_none(),
                "/{name} is in TUI_ONLY but also in the shared table"
            );
        }
    }

    #[test]
    fn new_remote_commands_lookup_with_scope_and_usage() {
        let cases: &[(&str, &str, Scope)] = &[
            ("sessions", "/sessions", Scope::Client),
            ("compact", "/compact", Scope::Session),
            (
                "effort",
                "/effort <low|medium|high|xhigh|max|off>",
                Scope::Session,
            ),
            ("plan", "/plan [task]", Scope::Session),
            ("build", "/build", Scope::Session),
            ("team", "/team [N] <task>", Scope::Session),
        ];
        for (name, usage, scope) in cases {
            let spec = slash_lookup(name).unwrap_or_else(|| panic!("/{name} missing"));
            assert_eq!(spec.usage, *usage, "usage for /{name}");
            assert_eq!(spec.scope, *scope, "scope for /{name}");
            assert!(!spec.about.is_empty(), "/{name} has no description");
            assert!(
                slash_help_lines().iter().any(|l| l.starts_with(usage)),
                "/{name} missing from /help"
            );
        }
        assert!(slash_lookup("nope").is_none());
    }

    #[test]
    fn suggestions_complete_the_new_commands() {
        assert_eq!(suggestions("/sess"), vec!["sessions"]);
        assert_eq!(suggestions("com"), vec!["compact"]);
        assert_eq!(suggestions("/eff"), vec!["effort"]);
        assert_eq!(suggestions("/pl"), vec!["plan"]);
        assert_eq!(suggestions("/bu"), vec!["build"]);
        assert_eq!(suggestions("/te"), vec!["team"]);
        // Shared prefixes keep canonical table order.
        assert_eq!(
            suggestions("/s"),
            vec!["sandbox", "sessions", "share", "settings"]
        );
        assert!(suggestions("/zzz").is_empty());
        assert_eq!(suggestions("").len(), SLASH_COMMANDS.len());
    }

    #[test]
    fn suggestions_match_aliases_and_return_each_command_once() {
        // Name and alias both match the prefix: one canonical row.
        assert_eq!(suggestions("/co"), vec!["compact", "review"]);
        assert_eq!(suggestions("/compr"), vec!["compact"]);
        assert_eq!(suggestions("/comp"), vec!["compact"]);
        assert_eq!(suggestions("/exit"), vec!["exit"]);
        // Alias-only prefixes return the canonical name.
        assert_eq!(suggestions("/th"), vec!["reasoning"]);
        assert_eq!(suggestions("/per"), vec!["agent"]);
        assert_eq!(suggestions("/pub"), vec!["share"]);
        assert_eq!(suggestions("/rc"), vec!["remote-control"]);
        assert_eq!(suggestions("/log"), vec!["auth"]);
        assert_eq!(suggestions("/code"), vec!["review"]);
        assert_eq!(suggestions("/work"), vec!["worktree"]);
        assert_eq!(suggestions("/q"), vec!["exit"]);
        assert_eq!(suggestions("?"), vec!["help"]);
        // Every command appears at most once for the empty token.
        let all = suggestions("");
        let mut unique = all.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), all.len());
    }

    #[test]
    fn canned_prompts_match_the_old_tui_text() {
        // Literal copies of the old TUI's task text (crates/modron-tui/src/
        // lib.rs, CodeReview / Btw / Plan / Learn arms, ~2570-2715).
        assert_eq!(
            REVIEW_TASK,
            "Review the working tree: run `git diff` (and `git status`), then report findings only \u{2014} bugs, risks, and gaps, each with file:line references. Do not fix anything."
        );
        assert_eq!(
            btw_task("why?"),
            "why?\n\n(Quick side question: answer directly from what you already know \u{2014} do not use tools.)"
        );
        assert_eq!(
            plan_task("add tests"),
            "add tests\n\n(Plan first: produce a concrete, step-by-step plan with file references. Do NOT edit any files yet \u{2014} wait for approval.)"
        );
        assert_eq!(
            learn_task("Fix the Parser!"),
            "Distill this session into a reusable skill. Write the file .modron/skills/fix-the-parser/SKILL.md with `---` frontmatter (name: fix-the-parser, description: one line) followed by a concise, step-by-step workflow capturing what actually worked here (commands, file paths, pitfalls). Create the directories. Then reply with just the path you wrote."
        );
        assert!(learn_task("").contains(".modron/skills/session-skill/SKILL.md"));
        assert_eq!(skill_slug("  a -- b  "), "a-b");
        assert_eq!(skill_slug(&"x".repeat(80)).len(), 48);
    }
}
