//! `read` tool (LEDGER T1) — file reading with line numbers and hard caps.

use crate::common::{arg_str, opt_usize, resolve_path, tool_err};
use crate::reduce::reduce_output;
use crate::validate::validate_args;
use async_trait::async_trait;
use modron_core::{Result, Tool, ToolCtx, ToolOutcome, ToolSpec};
use serde_json::{json, Value};

/// Hard cap on lines returned per call.
pub const MAX_LINES: usize = 2000;
/// Byte cap applied to the numbered output (elision marker appended beyond it).
pub const MAX_BYTES: usize = 48 * 1024;

/// Reads a UTF-8 text file and returns `cat -n`-style numbered lines
/// (`     1\t<line>`). Output is routed through [`crate::reduce::reduce_output`]
/// at 48KB.
pub struct ReadTool;

#[async_trait]
impl Tool for ReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read".into(),
            description: "Read a UTF-8 text file as cat -n style numbered lines. Args: path; \
                          optional 1-based offset and limit (default 2000, max 2000). Output \
                          capped at 48KB."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path (relative to working dir or absolute)."},
                    "offset": {"type": "integer", "description": "1-based first line to read."},
                    "limit": {"type": "integer", "description": "Max lines to read (default 2000, max 2000)."}
                },
                "required": ["path"]
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        const NAME: &str = "read";
        let spec = self.spec();
        if let Err(message) = validate_args(&spec, &args) {
            return Ok(tool_err(
                NAME,
                format!("invalid arguments: {message}"),
                MAX_BYTES,
            ));
        }
        let Some(raw_path) = arg_str(&args, "path") else {
            return Ok(tool_err(NAME, "missing `path`", MAX_BYTES));
        };
        let path = resolve_path(&ctx.working_dir, raw_path);
        let offset = opt_usize(&args, "offset").unwrap_or(1).max(1);
        let limit = opt_usize(&args, "limit")
            .unwrap_or(MAX_LINES)
            .clamp(1, MAX_LINES);

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
        // Read-before-edit gate (Phase 13): a successful read unlocks the
        // file for edit/write in this session.
        if let Some(engine) = &ctx.engine {
            engine.tools.read_gate.mark(&path);
        }

        let mut out = String::new();
        let mut shown = 0usize;
        for (idx, line) in content.lines().enumerate().skip(offset - 1) {
            if shown >= limit {
                break;
            }
            out.push_str(&format!("{:>6}\t{line}\n", idx + 1));
            shown += 1;
        }

        if shown == 0 {
            let total = content.lines().count();
            return Ok(ToolOutcome::ok(reduce_output(
                format!("no lines in range: file has {total} line(s), requested offset {offset}"),
                MAX_BYTES,
            )));
        }
        Ok(ToolOutcome::ok(reduce_output(out, MAX_BYTES)))
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
        ReadTool.execute(args, &ctx).await.expect("execute")
    }

    #[tokio::test]
    async fn reads_numbered_lines() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let out = run(json!({"path": "f.txt"}), dir.path()).await;
        assert!(!out.is_error);
        assert!(out.content.contains("     1\talpha"), "{out:?}");
        assert!(out.content.contains("     2\tbeta"), "{out:?}");
        assert!(out.content.contains("     3\tgamma"), "{out:?}");
    }

    #[tokio::test]
    async fn offset_and_limit_select_range() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "l1\nl2\nl3\nl4\nl5\n").unwrap();
        let out = run(
            json!({"path": "f.txt", "offset": 2, "limit": 2}),
            dir.path(),
        )
        .await;
        assert!(!out.is_error);
        assert_eq!(out.content, "     2\tl2\n     3\tl3");
    }

    #[tokio::test]
    async fn missing_file_is_error_outcome() {
        let dir = tempdir().unwrap();
        let out = run(json!({"path": "nope.txt"}), dir.path()).await;
        assert!(out.is_error, "expected error outcome, got {out:?}");
        assert!(out.content.contains("cannot read"), "{out:?}");
    }

    #[tokio::test]
    async fn large_file_truncates_at_cap() {
        let dir = tempdir().unwrap();
        // 3000 lines x ~40 chars ~= 126KB of raw content, well over 48KB.
        let body: String = (0..3000)
            .map(|i| format!("line {i:06} {}", "x".repeat(30)))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.path().join("big.txt"), body).unwrap();
        let out = run(json!({"path": "big.txt"}), dir.path()).await;
        assert!(
            !out.is_error,
            "cap is a normal elision, not an error: {out:?}"
        );
        assert!(out.content.contains("bytes elided]"), "{out:?}");
        assert!(
            out.content.len() < MAX_BYTES + 200,
            "len {}",
            out.content.len()
        );
    }

    #[tokio::test]
    async fn invalid_args_are_tool_error() {
        let dir = tempdir().unwrap();
        // Missing required `path` — must surface as error outcome, not panic.
        let out = run(json!({"limit": 5}), dir.path()).await;
        assert!(out.is_error, "{out:?}");
        assert!(
            out.content.contains("missing required argument `path`"),
            "{out:?}"
        );
        // Unknown key.
        let out = run(json!({"path": "f.txt", "mode": "zzz"}), dir.path()).await;
        assert!(
            out.is_error && out.content.contains("unknown argument `mode`"),
            "{out:?}"
        );
    }
}
