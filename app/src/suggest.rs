//! The composer's suggestion list: command suggestions for a leading `/`, and
//! `@file` completions for a live `@token` (terminal only: on the web `@` is
//! plain text). One list shows either (`chat::slash_list`) and the keys are
//! the same: Up and Down move, Tab completes, Esc hides it.
//!
//! The `@token` rule is the old TUI's (crates/modron-tui `at_token`): the `@`
//! starts a token (the start of the text, or after whitespace, so `a@b` is
//! not one), and the text after it holds no whitespace (the token ended).

use crate::slash;

/// The suggestions for one composer text.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Items {
    /// True for `@file` completions: Tab splices the path, Enter submits.
    pub file: bool,
    /// What a selection completes: a command name, or a workspace path.
    pub names: Vec<String>,
    /// The row text, without the selection marker: `usage - about`, or `@path`.
    pub rows: Vec<String>,
}

/// The suggestions the composer text shows. Empty when it shows none.
pub fn for_draft(draft: &str) -> Items {
    if let Some(items) = file_items(draft) {
        return items;
    }
    let entries = slash::suggestions_for(draft);
    Items {
        file: false,
        names: entries.iter().map(|e| e.name.to_string()).collect(),
        rows: entries.iter().map(|e| format!("{} - {}", e.usage, e.about)).collect(),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn file_items(draft: &str) -> Option<Items> {
    let (_, fragment) = at_token(draft)?;
    let names = crate::files::matches(&fragment);
    let rows = names.iter().map(|path| format!("@{path}")).collect();
    Some(Items { file: true, names, rows })
}

#[cfg(target_arch = "wasm32")]
fn file_items(_draft: &str) -> Option<Items> {
    None
}

/// The live `@` token: the byte index of its `@` and the fragment after it.
/// The last `@` of the text is the one considered. None when that `@` does
/// not start a token, or when whitespace follows it.
pub fn at_token(draft: &str) -> Option<(usize, String)> {
    let at = draft.rfind('@')?;
    if at > 0 && !draft[..at].ends_with(char::is_whitespace) {
        return None;
    }
    let rest = &draft[at + 1..];
    if rest.chars().any(char::is_whitespace) {
        return None;
    }
    Some((at, rest.to_string()))
}

/// The draft with its live `@token` replaced by `@path ` (Tab). The text
/// after the token is kept. None when the draft has no live token.
pub fn splice_file(draft: &str, path: &str) -> Option<String> {
    let (at, fragment) = at_token(draft)?;
    let end = at + 1 + fragment.len();
    Some(format!("{}@{path} {}", &draft[..at], &draft[end..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn at_must_start_a_token() {
        assert_eq!(at_token("@sr"), Some((0, "sr".into())));
        assert_eq!(at_token("fix @sr"), Some((4, "sr".into())));
        assert_eq!(at_token("mail a@b.c"), None);
        assert_eq!(at_token("@sr done"), None, "a finished token");
        assert_eq!(at_token("@"), Some((0, String::new())));
    }

    #[test]
    fn splice_replaces_the_token_and_keeps_the_rest() {
        assert_eq!(splice_file("@ma", "src/main.rs").as_deref(), Some("@src/main.rs "));
        assert_eq!(
            splice_file("look at @ma", "src/main.rs").as_deref(),
            Some("look at @src/main.rs ")
        );
        assert_eq!(splice_file("no token", "x"), None);
    }

    #[test]
    fn a_plain_slash_word_still_lists_commands() {
        let items = for_draft("/co");
        assert!(!items.file);
        assert_eq!(items.names, vec!["compact".to_string(), "context".to_string()]);
        assert_eq!(items.rows.len(), 2);
        assert!(items.rows[0].contains(" - "));
    }

    #[test]
    fn an_email_address_lists_nothing() {
        assert!(for_draft("mail a@b.c").names.is_empty());
    }
}
