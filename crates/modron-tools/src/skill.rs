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

    /// The skill index for the system prompt: `- name: description` per
    /// skill, byte-capped. Empty when no skills exist.
    pub fn index(&self) -> String {
        let mut entries: Vec<String> = Vec::new();
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
                entries.push(format!(
                    "- {}: {}",
                    name.unwrap_or_else(|| entry.file_name().to_string_lossy().to_string()),
                    description.unwrap_or_else(|| "(no description)".to_string())
                ));
            }
        }
        let mut out = entries.join("\n");
        if out.len() > 2048 {
            let cut: String = out.chars().take(2048).collect();
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
        None
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
            description: "Load the full instructions of a repo skill (SKILL.md). The index of \
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
