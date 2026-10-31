//! `write` tool (LEDGER T1) — create or overwrite text files.

use crate::common::{arg_str, opt_bool, resolve_path, tool_err};
use crate::reduce::reduce_output;
use crate::validate::validate_args;
use async_trait::async_trait;
use modron_core::{Result, Tool, ToolCtx, ToolOutcome, ToolSpec};
use serde_json::{json, Value};

const MAX_BYTES: usize = 1024;

/// Writes text content to a file, creating parent directories as needed.
/// Refuses to overwrite an existing file unless `overwrite: true`. Reports
/// bytes written.
pub struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write".into(),
            description: "Write text content to a file (parent dirs created). Refuses to \
                          overwrite an existing file unless overwrite is true."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path (relative to working dir or absolute)."},
                    "content": {"type": "string", "description": "Full file content."},
                    "overwrite": {"type": "boolean", "description": "Replace the file if it exists."}
                },
                "required": ["path", "content"]
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        const NAME: &str = "write";
        let spec = self.spec();
        if let Err(message) = validate_args(&spec, &args) {
            return Ok(tool_err(
                NAME,
                format!("invalid arguments: {message}"),
                MAX_BYTES,
            ));
        }
        let (Some(raw_path), Some(content)) = (arg_str(&args, "path"), arg_str(&args, "content"))
        else {
            return Ok(tool_err(NAME, "missing `path` or `content`", MAX_BYTES));
        };
        let overwrite = opt_bool(&args, "overwrite").unwrap_or(false);
        let path = resolve_path(&ctx.working_dir, raw_path);

        if tokio::fs::try_exists(&path).await.unwrap_or(false) && !overwrite {
            return Ok(tool_err(
                NAME,
                format!(
                    "{} already exists; pass overwrite: true to replace it",
                    path.display()
                ),
                MAX_BYTES,
            ));
        }
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(e) = tokio::fs::create_dir_all(parent).await {
                    return Ok(tool_err(
                        NAME,
                        format!("cannot create {}: {e}", parent.display()),
                        MAX_BYTES,
                    ));
                }
            }
        }
        if let Err(e) = tokio::fs::write(&path, content.as_bytes()).await {
            return Ok(tool_err(
                NAME,
                format!("cannot write {}: {e}", path.display()),
                MAX_BYTES,
            ));
        }
        Ok(ToolOutcome::ok(reduce_output(
            format!("wrote {} bytes to {}", content.len(), path.display()),
            MAX_BYTES,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    async fn run(args: Value, dir: &std::path::Path) -> ToolOutcome {
        let ctx = ToolCtx {
            sandbox: modron_core::SandboxPolicy::Off,
            working_dir: dir.to_path_buf(),
        };
        WriteTool.execute(args, &ctx).await.expect("execute")
    }

    #[tokio::test]
    async fn writes_new_file_and_reports_bytes() {
        let dir = tempdir().unwrap();
        let out = run(json!({"path": "new.txt", "content": "hello"}), dir.path()).await;
        assert!(!out.is_error, "{out:?}");
        assert!(out.content.contains("wrote 5 bytes to"), "{out:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("new.txt")).unwrap(),
            "hello"
        );
    }

    #[tokio::test]
    async fn refuses_overwrite_without_flag() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "original").unwrap();
        let out = run(
            json!({"path": "f.txt", "content": "replacement"}),
            dir.path(),
        )
        .await;
        assert!(out.is_error, "{out:?}");
        assert!(out.content.contains("already exists"), "{out:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "original",
            "file must be untouched"
        );
    }

    #[tokio::test]
    async fn overwrites_with_flag() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "original").unwrap();
        let out = run(
            json!({"path": "f.txt", "content": "replacement", "overwrite": true}),
            dir.path(),
        )
        .await;
        assert!(!out.is_error, "{out:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "replacement"
        );
    }

    #[tokio::test]
    async fn creates_parent_dirs() {
        let dir = tempdir().unwrap();
        let out = run(json!({"path": "a/b/c.txt", "content": "deep"}), dir.path()).await;
        assert!(!out.is_error, "{out:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a/b/c.txt")).unwrap(),
            "deep"
        );
    }
}
