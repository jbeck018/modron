//! The shared slash-command table (2026-11-11): one canonical list the
//! TUI, desktop GUI, and the web/mobile shell all serve. The names and
//! wordings are the contract (AGENTS.md parity): each client implements
//! what its surface can do and renders `/help` from the same strings,
//! so a command typed on any client means the same thing.
//!
//! The TUI also has local-only commands (theme, paste, issues) that stay
//! TUI-side; everything listed here is meaningful over the wire or in a
//! shared surface.

/// One slash command.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct SlashSpec {
    /// The word after `/` (no slash, no args).
    pub name: &'static str,
    /// Usage line, e.g. `/mode [ask|yolo|deny]`.
    pub usage: &'static str,
    /// One-line description (the shared copy clients render verbatim).
    pub about: &'static str,
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
        scope: Scope::Session,
    },
    SlashSpec {
        name: "mode",
        usage: "/mode [ask|yolo|deny]",
        about: "approval gate for tool runs (default ask)",
        scope: Scope::Session,
    },
    SlashSpec {
        name: "sandbox",
        usage: "/sandbox [off|seatbelt|bwrap]",
        about: "filesystem sandbox policy for future turns",
        scope: Scope::Session,
    },
    SlashSpec {
        name: "new",
        usage: "/new",
        about: "start a fresh chat",
        scope: Scope::Session,
    },
    SlashSpec {
        name: "fork",
        usage: "/fork",
        about: "branch the session log; the original is untouched",
        scope: Scope::Session,
    },
    SlashSpec {
        name: "tasks",
        usage: "/tasks",
        about: "status of background delegates and worktree tasks",
        scope: Scope::Session,
    },
    SlashSpec {
        name: "share",
        usage: "/share",
        about: "publish the transcript as a read-only web page",
        scope: Scope::Server,
    },
    SlashSpec {
        name: "remote-control",
        usage: "/remote-control [bridge-url]",
        about: "open a live tunnel other devices can watch and drive",
        scope: Scope::Server,
    },
    SlashSpec {
        name: "auth",
        usage: "/auth",
        about: "provider credentials and sign-in status",
        scope: Scope::Client,
    },
    SlashSpec {
        name: "settings",
        usage: "/settings",
        about: "open the settings view",
        scope: Scope::Client,
    },
    SlashSpec {
        name: "help",
        usage: "/help",
        about: "list the slash commands",
        scope: Scope::Client,
    },
];

/// Slash names only (for parsers).
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

/// Look up a command by the word after the slash.
pub fn slash_lookup(name: &str) -> Option<&'static SlashSpec> {
    SLASH_COMMANDS.iter().find(|c| c.name == name)
}

/// Completion candidates for a typed `/token` (prefix match - the
/// desktop's original semantics; canonical order). One copy now (the TSX
/// keeps its JS filter over the served table).
pub fn suggestions(token: &str) -> Vec<&'static str> {
    let token = token.trim_start_matches('/').to_lowercase();
    if token.is_empty() {
        return SLASH_COMMANDS.iter().map(|c| c.name).collect();
    }
    SLASH_COMMANDS
        .iter()
        .map(|c| c.name)
        .filter(|name| name.starts_with(&token))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
