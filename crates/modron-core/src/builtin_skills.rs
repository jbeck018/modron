//! Built-in methodology skills (ported from Jesse Vincent's superpowers,
//! MIT — github.com/obra/superpowers), embedded so every workspace has
//! them without repo-local files. Repo skills with the same name
//! OVERRIDE the built-in: users can reshape the methodology per repo.

/// (name, description, body) for every built-in skill.
pub const BUILTIN_SKILLS: &[(&str, &str, &str)] = &[
    (
        "brainstorming",
        "explore options with your partner before any creative work",
        include_str!("skills/brainstorming.md"),
    ),
    (
        "writing-plans",
        "turn a spec into bite-sized TDD tasks before touching code",
        include_str!("skills/writing-plans.md"),
    ),
    (
        "executing-plans",
        "execute a plan inline with the progress ledger as memory",
        include_str!("skills/executing-plans.md"),
    ),
    (
        "subagent-driven-development",
        "leader/worker: fresh delegate per task, per-task review, ledger",
        include_str!("skills/subagent-driven-development.md"),
    ),
    (
        "test-driven-development",
        "no production code without a failing test first",
        include_str!("skills/test-driven-development.md"),
    ),
    (
        "systematic-debugging",
        "root cause before any fix, always",
        include_str!("skills/systematic-debugging.md"),
    ),
    (
        "verification-before-completion",
        "evidence before any done/fixed/passing claim",
        include_str!("skills/verification-before-completion.md"),
    ),
    (
        "requesting-code-review",
        "fresh-reviewer packages for completed work",
        include_str!("skills/requesting-code-review.md"),
    ),
    (
        "receiving-code-review",
        "evaluate feedback technically; never blindly implement",
        include_str!("skills/receiving-code-review.md"),
    ),
    (
        "using-git-worktrees",
        "isolated workspaces before feature work",
        include_str!("skills/using-git-worktrees.md"),
    ),
    (
        "finishing-a-development-branch",
        "verify, review, and integrate finished branches",
        include_str!("skills/finishing-a-development-branch.md"),
    ),
    (
        "dispatching-parallel-agents",
        "run independent tasks concurrently, one delegate each",
        include_str!("skills/dispatching-parallel-agents.md"),
    ),
    (
        "writing-skills",
        "author and sharpen SKILL.md files",
        include_str!("skills/writing-skills.md"),
    ),
];

/// Lookup a built-in skill's body by name.
pub fn builtin_skill_body(name: &str) -> Option<&'static str> {
    BUILTIN_SKILLS
        .iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, _, body)| *body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_are_wellformed_and_unique() {
        let mut names: Vec<&str> = BUILTIN_SKILLS.iter().map(|(n, _, _)| *n).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "duplicate builtin skill name");
        assert!(total >= 12);
        for (name, description, body) in BUILTIN_SKILLS {
            assert!(!description.is_empty(), "{name} needs a description");
            assert!(
                body.contains(&format!("name: {name}")),
                "{name} frontmatter"
            );
            assert!(body.contains("superpowers"), "{name} keeps its attribution");
        }
    }
}
