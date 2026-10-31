//! `bash` tool (LEDGER T1) — shell execution with output caps and timeouts.

use crate::common::{arg_str, internal_tool_error, opt_u64, tool_err};
use crate::reduce::reduce_output;
use crate::validate::validate_args;
use async_trait::async_trait;
use modron_core::{Result, Tool, ToolCtx, ToolOutcome, ToolSpec};
use serde_json::{json, Value};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// Default command timeout.
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// Hard ceiling for `timeout_ms`.
pub const MAX_TIMEOUT_MS: u64 = 120_000;
/// Byte cap applied to command output before the exit-code line.
pub const MAX_BYTES: usize = 10 * 1024;

/// Runs a shell command (`sh -c`, `cmd /C` on Windows) with
/// `cwd = ctx.working_dir`. Captures stdout followed by stderr (blocking
/// readers on the tokio runtime), caps output at 10KB, and always appends an
/// `exit code: N` line. Non-zero exit codes and signal terminations are error
/// outcomes; timeouts kill the child and return an error outcome with the
/// partial output.
pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".into(),
            description:
                "Run a shell command (`sh -c`) in the working dir. Captures stdout+stderr \
                          and the exit code; output capped at 10KB. Optional timeout_ms (default \
                          30000, max 120000); timeouts kill the process."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Shell command to run."},
                    "timeout_ms": {"type": "integer", "description": "Timeout in ms (default 30000, max 120000)."}
                },
                "required": ["command"]
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        const NAME: &str = "bash";
        let spec = self.spec();
        if let Err(message) = validate_args(&spec, &args) {
            return Ok(tool_err(
                NAME,
                format!("invalid arguments: {message}"),
                MAX_BYTES,
            ));
        }
        let Some(command) = arg_str(&args, "command") else {
            return Ok(tool_err(NAME, "missing `command`", MAX_BYTES));
        };
        let timeout_ms = opt_u64(&args, "timeout_ms")
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .clamp(1, MAX_TIMEOUT_MS);

        // Sandbox routing (A2): under Seatbelt the whole shell runs inside
        // `sandbox-exec` (network denied, writes fenced to the workspace +
        // temp dirs). The sandboxed path cannot stream-pipe the same way, so
        // it uses one buffered capture; caps and exit-code reporting are
        // identical. Timeout handling is shared below.
        if ctx.sandbox == modron_core::SandboxPolicy::Seatbelt {
            let started = std::time::Instant::now();
            return match tokio::time::timeout(
                Duration::from_millis(timeout_ms),
                modron_core::sandbox::run_sandboxed(command, &ctx.working_dir),
            )
            .await
            {
                Ok(Ok(output)) => {
                    // stdout then stderr — same merge order as the piped path.
                    let so = String::from_utf8_lossy(&output.stdout).into_owned();
                    let se = String::from_utf8_lossy(&output.stderr).into_owned();
                    let text = match (so.is_empty(), se.is_empty()) {
                        (false, false) => format!("{so}\n{se}"),
                        (true, false) => se,
                        _ => so,
                    };
                    let code_line = match output.status.code() {
                        Some(code) => format!("exit code: {code}"),
                        None => "exit code: terminated by signal".to_string(),
                    };
                    let body = reduce_output(text, MAX_BYTES);
                    let content = if body.is_empty() {
                        code_line
                    } else {
                        format!("{body}\n{code_line}")
                    };
                    Ok(ToolOutcome {
                        content,
                        is_error: output.status.code().is_none_or(|c| c != 0),
                    })
                }
                Ok(Err(err)) => Err(internal_tool_error(NAME, err.to_string())),
                Err(_) => Ok(ToolOutcome::err(format!(
                    "command timed out after {} ms under the sandbox (ran {} ms); killed",
                    timeout_ms,
                    started.elapsed().as_millis()
                ))),
            };
        }

        let mut cmd = Command::new(shell());
        cmd.args(shell_args(command))
            .current_dir(&ctx.working_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .map_err(|e| internal_tool_error(NAME, format!("failed to spawn `{command}`: {e}")))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| internal_tool_error(NAME, "stdout was not captured"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| internal_tool_error(NAME, "stderr was not captured"))?;

        // Readers run concurrently with `wait` so full pipes never deadlock.
        let out_task = tokio::spawn(read_all(stdout));
        let err_task = tokio::spawn(read_all(stderr));

        // Bind first: the timeout future borrows `child`; the match must not.
        let waited = tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait()).await;
        match waited {
            Ok(Ok(status)) => {
                let text = merged_output(
                    join_read(NAME, out_task).await?,
                    join_read(NAME, err_task).await?,
                );
                let code_line = match status.code() {
                    Some(code) => format!("exit code: {code}"),
                    None => "exit code: terminated by signal".to_string(),
                };
                let body = reduce_output(text, MAX_BYTES);
                let content = if body.is_empty() {
                    code_line
                } else {
                    format!("{body}\n{code_line}")
                };
                Ok(ToolOutcome {
                    content,
                    is_error: status.code().is_none_or(|c| c != 0),
                })
            }
            Ok(Err(e)) => Err(internal_tool_error(NAME, format!("wait failed: {e}"))),
            Err(_elapsed) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                let text = merged_output(
                    join_read(NAME, out_task).await?,
                    join_read(NAME, err_task).await?,
                );
                let body = reduce_output(text, MAX_BYTES);
                Ok(ToolOutcome::err(format!(
                    "{body}\ncommand timed out after {timeout_ms} ms and was killed; partial output above"
                )))
            }
        }
    }
}

async fn read_all<R: tokio::io::AsyncRead + Unpin>(mut reader: R) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = reader.read_to_end(&mut buf).await;
    buf
}

async fn join_read(name: &str, task: tokio::task::JoinHandle<Vec<u8>>) -> Result<Vec<u8>> {
    task.await
        .map_err(|e| internal_tool_error(name, format!("output reader failed: {e}")))
}

/// Merges captured streams: stdout, then stderr on its own line if non-empty.
fn merged_output(stdout: Vec<u8>, stderr: Vec<u8>) -> String {
    let mut text = String::from_utf8_lossy(&stdout).into_owned();
    let err_text = String::from_utf8_lossy(&stderr);
    if !err_text.trim().is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&err_text);
    }
    text
}

#[cfg(unix)]
fn shell() -> &'static str {
    "sh"
}
#[cfg(unix)]
fn shell_args(command: &str) -> Vec<String> {
    vec!["-c".into(), command.into()]
}
#[cfg(windows)]
fn shell() -> &'static str {
    "cmd"
}
#[cfg(windows)]
fn shell_args(command: &str) -> Vec<String> {
    vec!["/C".into(), command.into()]
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
        BashTool.execute(args, &ctx).await.expect("execute")
    }

    #[tokio::test]
    async fn captures_output_and_exit_code() {
        let dir = tempdir().unwrap();
        let out = run(json!({"command": "echo hello"}), dir.path()).await;
        assert!(!out.is_error, "{out:?}");
        assert!(out.content.contains("hello"), "{out:?}");
        assert!(out.content.ends_with("exit code: 0"), "{out:?}");
    }

    #[tokio::test]
    async fn captures_stderr() {
        let dir = tempdir().unwrap();
        let out = run(json!({"command": "echo oops >&2"}), dir.path()).await;
        assert!(!out.is_error, "{out:?}");
        assert!(out.content.contains("oops"), "{out:?}");
    }

    #[tokio::test]
    async fn runs_in_working_dir() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("marker.txt"), "here").unwrap();
        let out = run(json!({"command": "cat marker.txt"}), dir.path()).await;
        assert!(!out.is_error, "{out:?}");
        assert!(out.content.contains("here"), "{out:?}");
    }

    #[tokio::test]
    async fn nonzero_exit_is_error_outcome() {
        let dir = tempdir().unwrap();
        let out = run(json!({"command": "exit 3"}), dir.path()).await;
        assert!(out.is_error, "{out:?}");
        assert!(out.content.ends_with("exit code: 3"), "{out:?}");
    }

    #[tokio::test]
    async fn oversized_output_is_capped_with_marker() {
        let dir = tempdir().unwrap();
        let out = run(json!({"command": "seq 1 100000"}), dir.path()).await;
        assert!(!out.is_error, "{out:?}");
        assert!(out.content.contains("bytes elided]"), "{out:?}");
        assert!(out.content.ends_with("exit code: 0"), "{out:?}");
        assert!(
            out.content.len() < MAX_BYTES + 200,
            "output should be near the 10KB cap, got {}",
            out.content.len()
        );
    }

    #[tokio::test]
    async fn timeout_kills_and_reports_partial_output() {
        let dir = tempdir().unwrap();
        let out = run(
            json!({"command": "echo partial; sleep 5", "timeout_ms": 300}),
            dir.path(),
        )
        .await;
        assert!(out.is_error, "{out:?}");
        assert!(out.content.contains("partial"), "{out:?}");
        assert!(out.content.contains("timed out after 300 ms"), "{out:?}");
    }

    #[tokio::test]
    async fn invalid_args_are_tool_error() {
        let dir = tempdir().unwrap();
        let out = run(json!({"command": 42}), dir.path()).await;
        assert!(out.is_error, "{out:?}");
        assert!(out.content.contains("must be of type string"), "{out:?}");
        let out = run(json!({}), dir.path()).await;
        assert!(
            out.is_error && out.content.contains("missing required argument `command`"),
            "{out:?}"
        );
    }
}
