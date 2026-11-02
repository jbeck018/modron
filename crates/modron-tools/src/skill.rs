//! The `skill` tool (Phase 4): loads the full body of a SKILL.md skill on
//! demand (the open standard — `.claude/skills/*/SKILL.md` and
//! `.modron/skills/*/SKILL.md` both work). The system prompt carries only
//! the index (name + description); bodies enter context only when called.

use crate::common::tool_err;
use crate::reduce::reduce_output;
use crate::validate::validate_args;
use modron_core::{Tool, ToolCtx, ToolOutcome, ToolSpec};
use std::path::PathBuf;

const NAME: &str = "skill";
const MAX_BYTES: usize = 8 * 1024;

pub struct SkillTool {
    /// Roots to scan: `<working_dir>/.modron/skills` and
    /// `<working_dir>/.claude/skills` are appended at registration.
    roots: Vec<PathBuf>,
}

impl SkillTool {
    pub fn for_working_dir(working_dir: &std::path::Path) -> Self {
        Self {
            roots: vec![
                working_dir.join(".modron").join("skills"),
                working_dir.join(".claude").join("skills"),
            ],
        }
    }

    /// The skill index: `- name: description` per skill, byte-capped.
    /// Repo skills first; built-in methodology skills follow unless a
    /// repo skill already owns the name.
    pub fn index(&self) -> String {
        let mut entries: Vec<(String, String)> = Vec::new();
        for root in &self.roots {
            let Ok(read_dir) = std::fs::read_dir(root) else {
                continue;
            };
            for entry in read_dir.flatten() {
                let skill_md = entry.path().join("SKILL.md");
                let Ok(text) = std::fs::read_to_string(&skill_md) else {
                    continue;
                };
                let (name, description) = parse_frontmatter(&text);
                entries.push((
                    name.unwrap_or_else(|| entry.file_name().to_string_lossy().to_string()),
                    description.unwrap_or_else(|| "(no description)".to_string()),
                ));
            }
        }
        for (name, description, _) in modron_core::builtin_skills::BUILTIN_SKILLS {
            if !entries.iter().any(|(n, _)| n == name) {
                entries.push((name.to_string(), description.to_string()));
            }
        }
        let mut out = entries
            .iter()
            .map(|(name, description)| format!("- {name}: {description}"))
            .collect::<Vec<_>>()
            .join("\n");
        if out.len() > 3072 {
            let cut: String = out.chars().take(3072).collect();
            out = format!("{cut}…");
        }
        out
    }

    fn load_body(&self, name: &str) -> Option<String> {
        for root in &self.roots {
            let path = root.join(name).join("SKILL.md");
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let (_, body) = split_frontmatter(&text);
            return Some(body);
        }
        // Built-in skills ship with the binary.
        modron_core::builtin_skills::builtin_skill_body(name).map(|text| split_frontmatter(text).1)
    }
}

/// Split a SKILL.md into (frontmatter yaml, body). No frontmatter →
/// (None, whole text).
fn split_frontmatter(text: &str) -> (Option<String>, String) {
    let mut lines = text.lines();
    if lines.next() == Some("---") {
        let mut fm = Vec::new();
        for line in lines.by_ref() {
            if line == "---" {
                return (Some(fm.join("\n")), lines.collect::<Vec<_>>().join("\n"));
            }
            fm.push(line);
        }
    }
    (None, text.to_string())
}

fn parse_frontmatter(text: &str) -> (Option<String>, Option<String>) {
    let (Some(fm), _) = split_frontmatter(text) else {
        return (None, None);
    };
    let mut name = None;
    let mut description = None;
    for line in fm.lines() {
        if let Some(v) = line.strip_prefix("name:") {
            name = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("description:") {
            description = Some(v.trim().to_string());
        }
    }
    (name, description)
}

#[async_trait::async_trait]
impl Tool for SkillTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: NAME.to_string(),
            description:
                "Load the full instructions of a skill (repo SKILL.md or built-in). The index of \
                          available skills is in your system prompt; call this before following \
                          a skill's workflow."
                    .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"name": {"type": "string", "description": "Skill directory name."}},
                "required": ["name"]
            }),
        }
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolCtx,
    ) -> modron_core::Result<ToolOutcome> {
        let spec = self.spec();
        if let Err(message) = validate_args(&spec, &args) {
            return Ok(tool_err(
                NAME,
                format!("invalid arguments: {message}"),
                MAX_BYTES,
            ));
        }
        let Some(name) = args.get("name").and_then(|v| v.as_str()) else {
            return Ok(tool_err(NAME, "missing `name`", MAX_BYTES));
        };
        // Path-traversal guard: skill names are directory names, nothing more.
        if name.contains('/') || name.contains("..") {
            return Ok(tool_err(NAME, "invalid skill name", MAX_BYTES));
        }
        match self.load_body(name) {
            Some(body) => Ok(ToolOutcome::ok(reduce_output(body, MAX_BYTES))),
            None => {
                let available = self.index();
                Ok(tool_err(
                    NAME,
                    format!("no skill named `{name}`\navailable:\n{available}"),
                    MAX_BYTES,
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let skill = dir.path().join(".claude").join("skills").join("release");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: release\ndescription: Cut a release: bump versions, tag, changelog.\n---\n1. cargo workspaces ...\n2. git tag ...\n",
        )
        .unwrap();
        dir
    }

    #[test]
    fn index_lists_name_and_description() {
        let dir = skill_dir();
        let tool = SkillTool::for_working_dir(dir.path());
        let index = tool.index();
        assert!(index.contains("- release: Cut a release"), "{index}");
    }

    #[tokio::test]
    async fn body_loads_without_frontmatter() {
        let dir = skill_dir();
        let tool = SkillTool::for_working_dir(dir.path());
        let outcome = tool
            .execute(serde_json::json!({"name": "release"}), &ToolCtx::new("."))
            .await
            .unwrap();
        assert!(!outcome.is_error);
        assert!(outcome.content.contains("git tag"));
        assert!(
            !outcome.content.contains("description:"),
            "frontmatter stripped"
        );
    }

    #[tokio::test]
    async fn unknown_name_lists_available_skills() {
        let dir = skill_dir();
        let tool = SkillTool::for_working_dir(dir.path());
        let outcome = tool
            .execute(serde_json::json!({"name": "nope"}), &ToolCtx::new("."))
            .await
            .unwrap();
        assert!(outcome.is_error);
        assert!(outcome.content.contains("- release:"));
    }

    #[tokio::test]
    async fn traversal_is_rejected() {
        let dir = skill_dir();
        let tool = SkillTool::for_working_dir(dir.path());
        let outcome = tool
            .execute(serde_json::json!({"name": "../../etc"}), &ToolCtx::new("."))
            .await
            .unwrap();
        assert!(outcome.is_error);
        assert!(outcome.content.contains("invalid skill name"));
    }
}

/// A4 — gallery compatibility: real community skills carry extra
/// frontmatter (license, allowed-tools, nested metadata), come from
/// `.modron/skills` too, and occasionally have no frontmatter at all. The
/// parser must tolerate all of it without inventing data.
#[cfg(test)]
mod gallery_compat_tests {
    use super::*;

    fn gallery_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        // Anthropic-style: extra top-level keys.
        let anthropic = dir.path().join(".claude").join("skills").join("pdf");
        std::fs::create_dir_all(&anthropic).unwrap();
        std::fs::write(
            anthropic.join("SKILL.md"),
            "---\nname: pdf\ndescription: Process PDFs: extraction, merging, generation.\nlicense: MIT\nallowed-tools: Read, Bash\nmetadata:\n  author: anthropic\n  version: 1.0\n---\nUse pypdf for extraction...\n",
        )
        .unwrap();
        // .modron location parity + nested YAML in frontmatter.
        let modron = dir.path().join(".modron").join("skills").join("deploy");
        std::fs::create_dir_all(&modron).unwrap();
        std::fs::write(
            modron.join("SKILL.md"),
            "---\nname: deploy\ndescription: Ship safely.\nmetadata:\n  owner: platform\n  tier: 1\n---\nRun the checklist...\n",
        )
        .unwrap();
        // No frontmatter: filename becomes the name, whole text is the body.
        let bare = dir.path().join(".claude").join("skills").join("scratch");
        std::fs::create_dir_all(&bare).unwrap();
        std::fs::write(bare.join("SKILL.md"), "Just do the thing.\n").unwrap();
        dir
    }

    #[test]
    fn extra_and_nested_frontmatter_keys_are_tolerated() {
        let dir = gallery_dir();
        let tool = SkillTool::for_working_dir(dir.path());
        let index = tool.index();
        assert!(index.contains("- pdf: Process PDFs"), "{index}");
        assert!(index.contains("- deploy: Ship safely."), "{index}");
        assert!(
            index.contains("- scratch:"),
            "no-frontmatter skill falls back to its folder name: {index}"
        );
        assert!(
            !index.contains("anthropic"),
            "metadata authors must not leak into the name: {index}"
        );
    }

    #[tokio::test]
    async fn bodies_load_from_both_roots_with_frontmatter_stripped() {
        let dir = gallery_dir();
        let tool = SkillTool::for_working_dir(dir.path());
        let pdf = tool
            .execute(serde_json::json!({"name": "pdf"}), &ToolCtx::new("."))
            .await
            .unwrap();
        assert!(pdf.content.contains("pypdf"));
        assert!(
            !pdf.content.contains("allowed-tools"),
            "frontmatter stripped"
        );

        let deploy = tool
            .execute(serde_json::json!({"name": "deploy"}), &ToolCtx::new("."))
            .await
            .unwrap();
        assert!(
            deploy.content.contains("checklist"),
            ".modron/skills root works"
        );
    }

    #[test]
    fn long_descriptions_are_capped_in_the_index() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..30 {
            let skill = dir
                .path()
                .join(".claude")
                .join("skills")
                .join(format!("skill{i}"));
            std::fs::create_dir_all(&skill).unwrap();
            std::fs::write(
                skill.join("SKILL.md"),
                format!(
                    "---\nname: skill{i}\ndescription: {}\n---\nbody",
                    "x".repeat(500)
                ),
            )
            .unwrap();
        }
        let tool = SkillTool::for_working_dir(dir.path());
        let index = tool.index();
        assert!(index.ends_with("…"), "index capped with an elision marker");
        assert!(
            index.len() < 3300,
            "index stays near the 3KB budget: {}",
            index.len()
        );
    }
}

#[cfg(test)]
mod builtin_tests {
    use super::*;

    #[tokio::test]
    async fn builtin_skill_bodies_load_without_a_repo() {
        let dir = tempfile::tempdir().unwrap(); // no skills dirs at all
        let tool = SkillTool::for_working_dir(dir.path());
        let outcome = tool
            .execute(
                serde_json::json!({"name": "subagent-driven-development"}),
                &ToolCtx::new("."),
            )
            .await
            .unwrap();
        assert!(!outcome.is_error, "{}", outcome.content);
        assert!(outcome.content.contains("leader"), "{}", outcome.content);
        assert!(
            !outcome.content.contains("description:"),
            "frontmatter stripped"
        );
        // The index lists the built-ins even with no repo skills.
        assert!(tool.index().contains("systematic-debugging"));
    }

    #[tokio::test]
    async fn repo_skill_overrides_the_builtin_body() {
        let dir = tempfile::tempdir().unwrap();
        let skill = dir
            .path()
            .join(".claude")
            .join("skills")
            .join("brainstorming");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: brainstorming\ndescription: Ours.\n---\nOUR PROCESS",
        )
        .unwrap();
        let tool = SkillTool::for_working_dir(dir.path());
        let outcome = tool
            .execute(
                serde_json::json!({"name": "brainstorming"}),
                &ToolCtx::new("."),
            )
            .await
            .unwrap();
        assert!(outcome.content.contains("OUR PROCESS"));
        assert!(
            !outcome.content.contains("superpowers"),
            "builtin body replaced"
        );
        assert_eq!(tool.index().matches("brainstorming").count(), 1);
    }
}
