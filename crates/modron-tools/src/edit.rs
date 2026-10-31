//! `edit` tool (LEDGER T1) — exact string replacement in text files.

use crate::common::{arg_str, opt_bool, resolve_path, tool_err};
use crate::reduce::reduce_output;
use crate::validate::validate_args;
use async_trait::async_trait;
use modron_core::{Result, Tool, ToolCtx, ToolOutcome, ToolSpec};
use serde_json::{json, Value};

const MAX_BYTES: usize = 1024;

/// Replaces an exact `old_string` with `new_string` in a UTF-8 text file.
/// `old_string` must appear exactly once unless `replace_all: true`; zero or
/// multiple matches without the flag are error outcomes naming the count.
pub struct EditTool;

#[async_trait]
impl Tool for EditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit".into(),
            description: "Exact string replacement in a UTF-8 text file. old_string must match \
                          exactly once unless replace_all is true; zero or multiple matches are \
                          errors naming the count."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path (relative to working dir or absolute)."},
                    "old_string": {"type": "string", "description": "Exact text to replace."},
                    "new_string": {"type": "string", "description": "Replacement text."},
                    "replace_all": {"type": "boolean", "description": "Replace every occurrence."}
                },
                "required": ["path", "old_string", "new_string"]
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        // Protected instruction sources (Phase: competitive review A3) —
        // the agent must not edit the files that instruct it. bash remains
        // the documented escape hatch (and is approval-gated).
        if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
            if is_protected_path(path) {
                return Ok(tool_err(
                    NAME,
                    format!(
                        "`{path}` is write-protected (instruction/memory source); \
                             edit it yourself outside the agent"
                    ),
                    MAX_BYTES,
                ));
            }
        }
        const NAME: &str = "edit";
        let spec = self.spec();
        if let Err(message) = validate_args(&spec, &args) {
            return Ok(tool_err(
                NAME,
                format!("invalid arguments: {message}"),
                MAX_BYTES,
            ));
        }
        let (Some(raw_path), Some(old), Some(new)) = (
            arg_str(&args, "path"),
            arg_str(&args, "old_string"),
            arg_str(&args, "new_string"),
        ) else {
            return Ok(tool_err(
                NAME,
                "missing `path`, `old_string` or `new_string`",
                MAX_BYTES,
            ));
        };
        let replace_all = opt_bool(&args, "replace_all").unwrap_or(false);
        let path = resolve_path(&ctx.working_dir, raw_path);

        let content = match tokio::fs::read_to_string(&path).await {
            Ok(content) => content,
            Err(e) => {
                return Ok(tool_err(
                    NAME,
                    format!("cannot read {}: {e}", path.display()),
                    MAX_BYTES,
                ))
            }
        };
        if old.is_empty() {
            return Ok(tool_err(NAME, "`old_string` must not be empty", MAX_BYTES));
        }
        let count = content.matches(old).count();
        if count == 0 {
            return Ok(tool_err(
                NAME,
                format!("`old_string` not found in {}", path.display()),
                MAX_BYTES,
            ));
        }
        if count > 1 && !replace_all {
            return Ok(tool_err(
                NAME,
                format!(
                    "`old_string` appears {count} times in {}; add surrounding context to make it \
                     unique or pass replace_all: true",
                    path.display()
                ),
                MAX_BYTES,
            ));
        }
        let replaced = if replace_all { count } else { 1 };
        let updated = if replace_all {
            content.replace(old, new)
        } else {
            content.replacen(old, new, 1)
        };
        if let Err(e) = tokio::fs::write(&path, updated.as_bytes()).await {
            return Ok(tool_err(
                NAME,
                format!("cannot write {}: {e}", path.display()),
                MAX_BYTES,
            ));
        }
        Ok(ToolOutcome::ok(reduce_output(
            format!("replaced {replaced} occurrence(s) in {}", path.display()),
            MAX_BYTES,
        )))
    }
}

/// Files the agent may not modify: instruction and memory sources.
fn is_protected_path(path: &str) -> bool {
    let p = std::path::Path::new(path);
    let name = p
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    if name == "AGENTS.md" || name == "CLAUDE.md" {
        return true;
    }
    p.components().any(|c| {
        let s = c.as_os_str().to_string_lossy();
        s == ".modron" || s == ".claude" || s == ".git"
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    async fn run(args: Value, dir: &std::path::Path) -> ToolOutcome {
        let ctx = ToolCtx {
            engine: None,
            sandbox: modron_core::SandboxPolicy::Off,
            working_dir: dir.to_path_buf(),
        };
        EditTool.execute(args, &ctx).await.expect("execute")
    }

    #[tokio::test]
    async fn replaces_single_occurrence() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("f.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        let out = run(
            json!({"path": "f.rs", "old_string": "fn a() {}", "new_string": "fn c() {}"}),
            dir.path(),
        )
        .await;
        assert!(!out.is_error, "{out:?}");
        assert!(out.content.contains("replaced 1 occurrence"), "{out:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.rs")).unwrap(),
            "fn c() {}\nfn b() {}\n"
        );
    }

    #[tokio::test]
    async fn zero_matches_is_error() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "abc\n").unwrap();
        let out = run(
            json!({"path": "f.txt", "old_string": "zzz", "new_string": "y"}),
            dir.path(),
        )
        .await;
        assert!(out.is_error, "{out:?}");
        assert!(out.content.contains("not found"), "{out:?}");
    }

    #[tokio::test]
    async fn multiple_matches_without_flag_is_error_naming_count() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "x x x\n").unwrap();
        let out = run(
            json!({"path": "f.txt", "old_string": "x", "new_string": "y"}),
            dir.path(),
        )
        .await;
        assert!(out.is_error, "{out:?}");
        assert!(out.content.contains("appears 3 times"), "{out:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "x x x\n"
        );
    }

    #[tokio::test]
    async fn replace_all_replaces_every_occurrence() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "x x x\n").unwrap();
        let out = run(
            json!({"path": "f.txt", "old_string": "x", "new_string": "y", "replace_all": true}),
            dir.path(),
        )
        .await;
        assert!(!out.is_error, "{out:?}");
        assert!(out.content.contains("replaced 3 occurrence"), "{out:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "y y y\n"
        );
    }

    #[tokio::test]
    async fn missing_file_is_error() {
        let dir = tempdir().unwrap();
        let out = run(
            json!({"path": "nope.txt", "old_string": "a", "new_string": "b"}),
            dir.path(),
        )
        .await;
        assert!(out.is_error, "{out:?}");
        assert!(out.content.contains("cannot read"), "{out:?}");
    }
}
