//! `read` tool (LEDGER T1) — file reading with line numbers and hard caps.

use crate::common::{
    arg_str, open_regular_file, opt_usize, resolve_path, tool_err, MAX_INPUT_BYTES,
};
use crate::reduce::reduce_output;
use crate::validate::validate_args;
use async_trait::async_trait;
use modron_core::{Result, Tool, ToolCtx, ToolOutcome, ToolSpec};
use serde_json::{json, Value};

/// Hard cap on lines returned per call.
pub const MAX_LINES: usize = 2000;
/// Byte cap applied to the numbered output (elision marker appended beyond it).
pub const MAX_BYTES: usize = 48 * 1024;
/// Per-line cap on the numbered output; a longer line is cut and marked.
pub const MAX_LINE_BYTES: usize = 64 * 1024;

/// Caps one line at [`MAX_LINE_BYTES`] on a char boundary, appending a marker
/// when cut. Lines within the cap are returned unchanged.
fn cap_line(line: &str) -> std::borrow::Cow<'_, str> {
    if line.len() <= MAX_LINE_BYTES {
        return line.into();
    }
    let mut end = MAX_LINE_BYTES;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} … [line truncated at 64 KiB]", &line[..end]).into()
}

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

        // Only regular files are opened (devices and FIFOs would hang or grow
        // without bound). Reads are capped at MAX_INPUT_BYTES in total; one
        // extra byte is read so an oversize file is detected, not silently cut.
        let file = match open_regular_file(&path) {
            Ok(file) => file,
            Err(e) => {
                return Ok(tool_err(
                    NAME,
                    format!("cannot read {}: {e}", path.display()),
                    MAX_BYTES,
                ))
            }
        };
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        // Read-before-edit gate (Phase 13): a successful read unlocks the
        // file for edit/write in this session.
        if let Some(engine) = &ctx.engine {
            engine.tools.read_gate.mark(&path);
        }

        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        if let Err(e) = tokio::fs::File::from_std(file)
            .take(MAX_INPUT_BYTES + 1)
            .read_to_end(&mut buf)
            .await
        {
            return Ok(tool_err(
                NAME,
                format!("cannot read {}: {e}", path.display()),
                MAX_BYTES,
            ));
        }
        let truncated = buf.len() as u64 > MAX_INPUT_BYTES;
        if truncated {
            buf.truncate(MAX_INPUT_BYTES as usize);
        }
        let text: &str = match std::str::from_utf8(&buf) {
            Ok(text) => text,
            // The input cut may split a trailing character: keep the valid prefix.
            Err(e) if truncated && e.error_len().is_none() => {
                std::str::from_utf8(&buf[..e.valid_up_to()]).unwrap_or("")
            }
            Err(_) => {
                return Ok(tool_err(
                    NAME,
                    format!(
                        "binary file ({size} bytes): not valid UTF-8 text, \
                         use bash to inspect it: {}",
                        path.display()
                    ),
                    MAX_BYTES,
                ))
            }
        };

        // Lines are split like `str::lines` (LF or CRLF). Each line is capped
        // at MAX_LINE_BYTES; a longer line is cut and marked, not an error.
        let mut out = String::new();
        let mut shown = 0usize;
        let mut idx = 0usize;
        for line in text.lines() {
            idx += 1;
            if idx < offset {
                continue;
            }
            if shown >= limit {
                break;
            }
            out.push_str(&format!("{:>6}\t{}\n", idx, cap_line(line)));
            shown += 1;
        }
        let input_note = if truncated {
            format!(" (only the first {} MiB were read)", MAX_INPUT_BYTES >> 20)
        } else {
            String::new()
        };

        if shown == 0 {
            let total = idx;
            return Ok(ToolOutcome::ok(reduce_output(
                format!(
                    "no lines in range: file has {total} line(s){input_note}, requested offset {offset}"
                ),
                MAX_BYTES,
            )));
        }
        if truncated {
            out.push_str(&format!(
                "… [input truncated: only the first {} MiB were read]\n",
                MAX_INPUT_BYTES >> 20
            ));
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

    #[tokio::test]
    async fn device_file_is_refused_quickly() {
        let dir = tempdir().unwrap();
        // /dev/zero is endless: any attempt to read it would never return.
        let out = run(json!({"path": "/dev/zero"}), dir.path()).await;
        assert!(out.is_error, "{out:?}");
        assert!(out.content.contains("not a regular file"), "{out:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fifo_is_refused_quickly() {
        let dir = tempdir().unwrap();
        let fifo = dir.path().join("pipe");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo");
        assert!(status.success());
        // A FIFO with no writer would block an open-and-read forever.
        let out = run(json!({"path": "pipe"}), dir.path()).await;
        assert!(out.is_error, "{out:?}");
        assert!(out.content.contains("not a regular file"), "{out:?}");
    }

    #[tokio::test]
    async fn long_line_is_truncated_not_an_error() {
        let dir = tempdir().unwrap();
        let body = format!("{}\nnext line\n", "a".repeat(200 * 1024));
        std::fs::write(dir.path().join("long.txt"), body).unwrap();
        let out = run(json!({"path": "long.txt"}), dir.path()).await;
        assert!(!out.is_error, "{out:?}");
        assert!(out.content.starts_with("     1\taaaa"), "{out:?}");
        assert!(
            out.content.len() <= MAX_BYTES + 200,
            "len {}",
            out.content.len()
        );
        // The 48 KiB output cap elides the cut line's tail, so the visible
        // marker is the output elision; the second line is beyond it.
        assert!(out.content.contains("bytes elided]"), "{out:?}");
    }

    #[tokio::test]
    async fn over_long_line_is_cut_at_64_kib_with_marker() {
        let dir = tempdir().unwrap();
        let body = format!("{}\nnext line\n", "b".repeat(70 * 1024));
        std::fs::write(dir.path().join("l.txt"), body).unwrap();
        // Offset 2 shows only the line after the long one: no output cap in play.
        let out = run(json!({"path": "l.txt", "offset": 2}), dir.path()).await;
        assert_eq!(out.content, "     2\tnext line");
        // The 48 KiB output cap then elides the rest of the 64 KiB line.
        let out = run(json!({"path": "l.txt", "limit": 1}), dir.path()).await;
        assert!(!out.is_error, "{out:?}");
        assert!(out.content.starts_with("     1\tbbbb"), "{out:?}");
        assert!(out.content.contains("bytes elided]"), "{out:?}");
        assert!(
            out.content.len() <= MAX_BYTES + 200,
            "len {}",
            out.content.len()
        );
    }

    #[test]
    fn cap_line_cuts_at_64_kib_on_char_boundary() {
        let short = "short";
        assert_eq!(cap_line(short), "short");
        let long = "a".repeat(MAX_LINE_BYTES + 10);
        let capped = cap_line(&long);
        assert!(capped.starts_with(&"a".repeat(MAX_LINE_BYTES)), "{capped}");
        assert!(capped.ends_with("[line truncated at 64 KiB]"), "{capped}");
        // A multi-byte character straddling the cap must not be split.
        let multi = format!("{}é", "a".repeat(MAX_LINE_BYTES - 1));
        let capped = cap_line(&multi);
        assert!(
            capped.starts_with(&"a".repeat(MAX_LINE_BYTES - 1)),
            "{capped}"
        );
        assert!(capped.ends_with("[line truncated at 64 KiB]"), "{capped}");
    }

    #[tokio::test]
    async fn binary_file_reports_binary_not_zero_lines() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("blob.bin"),
            [0xff_u8, 0xfe, 0x00, 0x01, 0x80],
        )
        .unwrap();
        let out = run(json!({"path": "blob.bin"}), dir.path()).await;
        assert!(out.is_error, "{out:?}");
        assert!(out.content.contains("binary file (5 bytes)"), "{out:?}");
        assert!(!out.content.contains("0 lines"), "{out:?}");
    }

    #[tokio::test]
    async fn oversize_file_is_capped_and_marked() {
        let dir = tempdir().unwrap();
        // 12 MiB of short lines: only the first 10 MiB may be read.
        let body = "x\n".repeat(6 * 1024 * 1024);
        std::fs::write(dir.path().join("huge.txt"), body).unwrap();
        let out = run(
            json!({"path": "huge.txt", "offset": 2, "limit": 1}),
            dir.path(),
        )
        .await;
        assert!(!out.is_error, "{out:?}");
        assert!(out.content.starts_with("     2\tx\n"), "{out:?}");
        assert!(out.content.contains("input truncated"), "{out:?}");
        let out = run(
            json!({"path": "huge.txt", "offset": 6_000_000, "limit": 1}),
            dir.path(),
        )
        .await;
        assert!(
            out.content.contains("only the first 10 MiB were read"),
            "{out:?}"
        );
    }
}
