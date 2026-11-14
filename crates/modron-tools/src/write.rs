//! `write` tool (LEDGER T1) — create or overwrite text files.

use crate::common::{arg_str, check_write_path, opt_bool, resolve_path, tool_err};
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
        // Protected instruction sources (Phase: competitive review A3) and
        // workspace containment (D4): checked on the resolved path, so
        // symlinks, `..` and letter case cannot route around it. bash remains
        // the documented escape hatch (and is approval-gated).
        let resolved = match args.get("path").and_then(|v| v.as_str()) {
            Some(raw) => match check_write_path(&ctx.working_dir, raw) {
                Ok(path) => Some(path),
                Err(message) => return Ok(tool_err(NAME, message, MAX_BYTES)),
            },
            None => None,
        };
        let spec = self.spec();
        if let Err(message) = validate_args(&spec, &args) {
            return Ok(tool_err(
                NAME,
                format!("invalid arguments: {message}"),
                MAX_BYTES,
            ));
        }
        // Hard read-before-edit gate (Phase 13): overwriting an existing
        // file the session has not read is blocked; brand-new files are fine.
        // The gate keys on the lexical path, the same key `read` records.
        if let (Some(engine), Some(raw_path)) =
            (&ctx.engine, args.get("path").and_then(|v| v.as_str()))
        {
            let path = resolve_path(&ctx.working_dir, raw_path);
            if path.is_file() && !engine.tools.read_gate.seen_in(ctx.session_id, &path) {
                return Ok(tool_err(
                    NAME,
                    format!(
                        "read-before-edit gate: read `{raw_path}` first — overwriting a file the \
                         session has not read is blocked",
                    ),
                    MAX_BYTES,
                ));
            }
        }
        let (Some(content), Some(path)) = (arg_str(&args, "content"), resolved) else {
            return Ok(tool_err(NAME, "missing `path` or `content`", MAX_BYTES));
        };
        let overwrite = opt_bool(&args, "overwrite").unwrap_or(false);

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
        if !overwrite {
            // create_new is the race-free exists-check: a file appearing
            // between check and write is never clobbered.
            match tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .await
            {
                Ok(mut file) => {
                    use tokio::io::AsyncWriteExt as _;
                    if let Err(e) = file.write_all(content.as_bytes()).await {
                        return Ok(tool_err(
                            NAME,
                            format!("cannot write {}: {e}", path.display()),
                            MAX_BYTES,
                        ));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    return Ok(tool_err(
                        NAME,
                        format!(
                            "{} already exists; pass overwrite: true to replace it",
                            path.display()
                        ),
                        MAX_BYTES,
                    ));
                }
                Err(e) => {
                    return Ok(tool_err(
                        NAME,
                        format!("cannot write {}: {e}", path.display()),
                        MAX_BYTES,
                    ));
                }
            }
        } else if let Err(e) = crate::common::write_atomic(&path, content.as_bytes()).await {
            return Ok(tool_err(
                NAME,
                format!("cannot write {}: {e}", path.display()),
                MAX_BYTES,
            ));
        }
        // Live LSP (Phase 14): full overwrites re-lint like edits — without
        // this push, diagnostics go stale after a `write`.
        // D6: language servers run repo code outside the sandbox, so they
        // are not started while one is active.
        if matches!(ctx.sandbox, modron_core::SandboxPolicy::Off) {
            let lsp_path = path.clone();
            let lsp_content = content.to_string();
            let lsp_root = ctx.working_dir.clone();
            let _ = tokio::task::spawn_blocking(move || {
                crate::lsp::did_write(&lsp_path, &lsp_content, &lsp_root)
            })
            .await;
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
            engine: None,
            sandbox: modron_core::SandboxPolicy::Off,
            working_dir: dir.to_path_buf(),
            session_id: None,
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

#[cfg(test)]
mod protection_tests {
    use super::*;

    #[tokio::test]
    async fn protected_instruction_files_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        for path in [
            "AGENTS.md",
            "docs/CLAUDE.md",
            ".claude/skills/x.md",
            ".modron/config.toml",
        ] {
            let outcome = WriteTool
                .execute(
                    serde_json::json!({"path": path, "content": "hijacked"}),
                    &ToolCtx::new(dir.path()),
                )
                .await
                .unwrap();
            assert!(outcome.is_error, "{path} must be refused");
            assert!(
                outcome.content.contains("write-protected"),
                "{path}: {}",
                outcome.content
            );
        }
        // Unrelated files still work.
        let ok = WriteTool
            .execute(
                serde_json::json!({"path": "src/main.rs", "content": "ok"}),
                &ToolCtx::new(dir.path()),
            )
            .await
            .unwrap();
        assert!(!ok.is_error);
    }

    #[tokio::test]
    async fn protected_names_match_case_insensitively() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".GIT/hooks")).unwrap();
        std::fs::create_dir_all(dir.path().join(".Modron")).unwrap();
        for path in [
            ".GIT/hooks/x",
            "agents.md",
            ".Modron/config.toml",
            "claude.md",
        ] {
            let outcome = WriteTool
                .execute(
                    serde_json::json!({"path": path, "content": "hijacked", "overwrite": true}),
                    &ToolCtx::new(dir.path()),
                )
                .await
                .unwrap();
            assert!(
                outcome.is_error,
                "{path} must be refused: {}",
                outcome.content
            );
            assert!(
                outcome.content.contains("write-protected"),
                "{path}: {}",
                outcome.content
            );
        }
        assert!(!dir.path().join(".GIT/hooks/x").exists());
        assert!(!dir.path().join("agents.md").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_through_directory_symlink_into_git_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git/hooks")).unwrap();
        std::os::unix::fs::symlink(".git/hooks", dir.path().join("docs")).unwrap();

        let outcome = WriteTool
            .execute(
                serde_json::json!({"path": "docs/pre-commit", "content": "evil"}),
                &ToolCtx::new(dir.path()),
            )
            .await
            .unwrap();

        assert!(outcome.is_error, "{}", outcome.content);
        assert!(
            outcome.content.contains("write-protected"),
            "{}",
            outcome.content
        );
        assert!(!dir.path().join(".git/hooks/pre-commit").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn overwrite_through_symlink_to_outside_file_is_refused() {
        let ws = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join("bashrc");
        std::fs::write(&victim, "original").unwrap();
        std::os::unix::fs::symlink(&victim, ws.path().join("notes")).unwrap();

        let outcome = WriteTool
            .execute(
                serde_json::json!({"path": "notes", "content": "evil", "overwrite": true}),
                &ToolCtx::new(ws.path()),
            )
            .await
            .unwrap();

        assert!(outcome.is_error, "{}", outcome.content);
        assert!(
            outcome
                .content
                .contains("outside the workspace through a symlink"),
            "{}",
            outcome.content
        );
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "original");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn overwrite_through_symlink_inside_workspace_still_works() {
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("real.txt"), "old").unwrap();
        std::os::unix::fs::symlink("real.txt", ws.path().join("link.txt")).unwrap();

        let outcome = WriteTool
            .execute(
                serde_json::json!({"path": "link.txt", "content": "new", "overwrite": true}),
                &ToolCtx::new(ws.path()),
            )
            .await
            .unwrap();

        assert!(!outcome.is_error, "{}", outcome.content);
        assert_eq!(
            std::fs::read_to_string(ws.path().join("real.txt")).unwrap(),
            "new"
        );
        assert!(std::fs::symlink_metadata(ws.path().join("link.txt"))
            .unwrap()
            .file_type()
            .is_symlink());
    }
}
