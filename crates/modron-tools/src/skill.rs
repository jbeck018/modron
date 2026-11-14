//! The `skill` tool (Phase 4): loads the full body of a SKILL.md skill on
//! demand (the open standard — `.claude/skills/*/SKILL.md` and
//! `.modron/skills/*/SKILL.md` both work). The system prompt carries only
//! the index (name + description); bodies enter context only when called.

use crate::common::{open_regular_file, tool_err};
use crate::reduce::reduce_output;
use crate::validate::validate_args;
use modron_core::{Tool, ToolCtx, ToolOutcome, ToolSpec};
use std::io::Read;
use std::path::{Path, PathBuf};

const NAME: &str = "skill";
const MAX_BYTES: usize = 8 * 1024;
/// Hard cap on one SKILL.md read (index and body alike).
const MAX_SKILL_BYTES: usize = 256 * 1024;
/// Appended to a body cut at `MAX_SKILL_BYTES`.
const SKILL_TRUNCATED_MARKER: &str = "\n[skill truncated]";

/// Reads one SKILL.md through the bounded regular-file gate: symlinks are
/// followed, but devices, FIFOs and directories are refused before opening,
/// and at most `MAX_SKILL_BYTES` are read. A cut body is cut on a UTF-8 char
/// boundary and carries `SKILL_TRUNCATED_MARKER`.
fn read_skill_text(path: &Path) -> Option<String> {
    let file = open_regular_file(path).ok()?;
    let mut buf = Vec::new();
    file.take(MAX_SKILL_BYTES as u64 + 1)
        .read_to_end(&mut buf)
        .ok()?;
    let truncated = buf.len() > MAX_SKILL_BYTES;
    if truncated {
        buf.truncate(MAX_SKILL_BYTES);
    }
    let mut text = match String::from_utf8(buf) {
        Ok(text) => text,
        // A cut can split a multi-byte character: keep the valid prefix.
        Err(e) if truncated && e.utf8_error().error_len().is_none() => {
            let valid = e.utf8_error().valid_up_to();
            let mut bytes = e.into_bytes();
            bytes.truncate(valid);
            String::from_utf8(bytes).ok()?
        }
        Err(_) => return None,
    };
    if truncated {
        text.push_str(SKILL_TRUNCATED_MARKER);
    }
    Some(text)
}

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
                let Some(text) = read_skill_text(&skill_md) else {
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
            // Fast path: the name IS the directory name.
            let path = root.join(name).join("SKILL.md");
            if let Some(text) = read_skill_text(&path) {
                let (_, body) = split_frontmatter(&text);
                return Some(body);
            }
            // The index advertises FRONTMATTER names — a repo skill whose
            // frontmatter name differs from its folder must still load by
            // that name (and keep overriding a same-named builtin).
            let Ok(read_dir) = std::fs::read_dir(root) else {
                continue;
            };
            for entry in read_dir.flatten() {
                let skill_md = entry.path().join("SKILL.md");
                let Some(text) = read_skill_text(&skill_md) else {
                    continue;
                };
                if parse_frontmatter(&text).0.as_deref() == Some(name) {
                    let (_, body) = split_frontmatter(&text);
                    return Some(body);
                }
            }
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
        ctx: &ToolCtx,
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
        // Session-cwd roots first: the engine's system prompt advertises
        // skills from the SESSION working dir (per-session ACP cwd), which
        // can differ from the launch dir this tool was registered with.
        let session_roots = [
            ctx.working_dir.join(".modron").join("skills"),
            ctx.working_dir.join(".claude").join("skills"),
        ];
        let body = session_roots
            .iter()
            .filter_map(|root| read_skill_body(root, name))
            .next()
            .or_else(|| self.load_body(name));
        match body {
            Some(body) => Ok(ToolOutcome::ok(finish_body(body))),
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

/// Model-facing body: reduced to `MAX_BYTES`, but a body cut at the read cap
/// keeps its `[skill truncated]` marker visible (the reduction budget leaves
/// room for it).
fn finish_body(body: String) -> String {
    match body.strip_suffix(SKILL_TRUNCATED_MARKER) {
        Some(head) => format!(
            "{}{SKILL_TRUNCATED_MARKER}",
            reduce_output(head, MAX_BYTES - SKILL_TRUNCATED_MARKER.len())
        ),
        None => reduce_output(body, MAX_BYTES),
    }
}

/// Read one skill body from a single root (frontmatter stripped), if the
/// directory exists there.
fn read_skill_body(root: &Path, name: &str) -> Option<String> {
    let text = read_skill_text(&root.join(name).join("SKILL.md"))?;
    let (_, body) = split_frontmatter(&text);
    Some(body)
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
    async fn body_loads_from_the_session_working_dir() {
        // ACP pins a per-session cwd: the tool was registered against the
        // launch dir, but the SESSION's repo skills must still load.
        let session = tempfile::tempdir().unwrap();
        let skill = session.path().join(".modron").join("skills").join("local");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: local\ndescription: session-local\n---\nsession-local body\n",
        )
        .unwrap();
        let other = tempfile::tempdir().unwrap();
        let tool = SkillTool::for_working_dir(other.path());
        let outcome = tool
            .execute(
                serde_json::json!({"name": "local"}),
                &ToolCtx::new(session.path()),
            )
            .await
            .unwrap();
        assert!(!outcome.is_error);
        assert!(
            outcome.content.contains("session-local body"),
            "{outcome:?}"
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

#[cfg(all(test, unix))]
mod bounded_read_tests {
    use super::*;
    use std::time::Duration;

    /// Runs `execute` on a worker thread so a hang fails the test instead of
    /// blocking the whole test binary.
    fn execute_or_hang(root: std::path::PathBuf, name: &'static str) -> ToolOutcome {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let tool = SkillTool::for_working_dir(&root);
            let outcome = rt
                .block_on(tool.execute(serde_json::json!({ "name": name }), &ToolCtx::new(&root)));
            let _ = tx.send(outcome.unwrap());
        });
        rx.recv_timeout(Duration::from_secs(10))
            .expect("skill tool hung on a non-regular SKILL.md")
    }

    #[test]
    fn skill_md_symlinked_to_dev_zero_is_refused_without_hanging() {
        let dir = tempfile::tempdir().unwrap();
        let skill = dir.path().join(".claude").join("skills").join("devnull");
        std::fs::create_dir_all(&skill).unwrap();
        std::os::unix::fs::symlink("/dev/zero", skill.join("SKILL.md")).unwrap();
        let outcome = execute_or_hang(dir.path().to_path_buf(), "devnull");
        assert!(outcome.is_error, "{outcome:?}");
    }

    #[test]
    fn oversized_skill_body_is_truncated_on_a_char_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let skill = dir.path().join(".claude").join("skills").join("big");
        std::fs::create_dir_all(&skill).unwrap();
        // 11 ASCII bytes then 2-byte chars: the 256 KiB cut lands mid-character.
        let body = format!("{}{}", "x".repeat(11), "é".repeat(200_000));
        std::fs::write(
            skill.join("SKILL.md"),
            format!("---\nname: big\ndescription: big\n---\n{body}"),
        )
        .unwrap();
        let outcome = execute_or_hang(dir.path().to_path_buf(), "big");
        assert!(!outcome.is_error, "{outcome:?}");
        assert!(
            outcome.content.ends_with("[skill truncated]"),
            "tail: {}",
            &outcome.content[outcome.content.len().saturating_sub(120)..]
        );
    }

    #[test]
    fn normal_skill_is_not_marked_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let skill = dir.path().join(".claude").join("skills").join("small");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: small\ndescription: small\n---\nstep one\n",
        )
        .unwrap();
        let outcome = execute_or_hang(dir.path().to_path_buf(), "small");
        assert!(!outcome.is_error, "{outcome:?}");
        assert!(outcome.content.contains("step one"), "{outcome:?}");
        assert!(!outcome.content.contains("truncated"), "{outcome:?}");
    }
}
