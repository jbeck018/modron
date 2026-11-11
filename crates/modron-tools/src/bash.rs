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
                          30000, max 120000); timeouts kill the process. Long-running services \
                          (servers, daemons) MUST be started fully detached so they survive the \
                          call: `setsid nohup CMD >log 2>&1 </dev/null &` — attached children are \
                          killed with the process group when the call times out."
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
                    let body = reduce_output(relay_oversized(&text), MAX_BYTES);
                    let content = if body.is_empty() {
                        code_line
                    } else {
                        format!("{body}\n{code_line}")
                    };
                    Ok(ToolOutcome {
                        content,
                        is_error: output.status.code().is_none_or(|c| c != 0),
                        usage: None,
                        meta: None,
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
        // Own process group: on timeout the WHOLE tree (grandchildren like
        // `cargo`, pipelines, `cmd &`) is signaled, not just the `sh`.
        #[cfg(unix)]
        cmd.process_group(0);

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
                let body = reduce_output(relay_oversized(&text), MAX_BYTES);
                let content = if body.is_empty() {
                    code_line
                } else {
                    format!("{body}\n{code_line}")
                };
                Ok(ToolOutcome {
                    content,
                    is_error: status.code().is_none_or(|c| c != 0),
                    usage: None,
                    meta: None,
                })
            }
            Ok(Err(e)) => Err(internal_tool_error(NAME, format!("wait failed: {e}"))),
            Err(_elapsed) => {
                kill_child_tree(&mut child).await;
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

/// Kill the spawned shell AND its process group (grandchildren included),
/// then reap. `child.kill()` alone would leave `cargo`, pipelines, and
/// background spawns running as orphans.
#[cfg(unix)]
async fn kill_child_tree(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        // Negative pid = the whole group (we spawned with group 0).
        unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}
#[cfg(not(unix))]
async fn kill_child_tree(child: &mut tokio::process::Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}

/// Phase 13 token-efficient I/O: output above this size is relayed to disk
/// and the model sees only a head+tail summary — the full text stays one
/// `read` away instead of flooding the context window.
pub const RELAY_THRESHOLD_BYTES: usize = 8 * 1024;
const RELAY_HEAD_LINES: usize = 30;
const RELAY_TAIL_LINES: usize = 15;

/// Relay oversized output to a temp file and return a head+tail summary
/// with the relay path. Under the threshold, the text passes through
/// unchanged.
fn relay_oversized(text: &str) -> String {
    if text.len() <= RELAY_THRESHOLD_BYTES {
        return text.to_string();
    }
    let dir = std::env::temp_dir().join("modron-relay");
    // 0700: the relay dir holds command output and its name is guessable —
    // other local users must not be able to pre-create or write inside it
    // (symlink attacks on the predictable file name).
    let _ = std::fs::create_dir_all(&dir);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    let name = format!("{}-{}.txt", chrono_now_compact(), fnv8(text.as_bytes()),);
    let path = dir.join(name);
    if std::fs::write(&path, text).is_err() {
        // Relay is best-effort: fall back to the old hard cap.
        return text.to_string();
    }
    let lines: Vec<&str> = text.lines().collect();
    let head: Vec<&str> = lines.iter().take(RELAY_HEAD_LINES).copied().collect();
    let tail: Vec<&str> = lines
        .iter()
        .rev()
        .take(RELAY_TAIL_LINES)
        .rev()
        .copied()
        .collect();
    format!(
        "{}\n… [+{} of {} lines relayed to {} — read it with the `read` tool] …\n{}",
        head.join("\n"),
        lines.len().saturating_sub(head.len() + tail.len()),
        lines.len(),
        path.display(),
        tail.join("\n"),
    )
}

fn chrono_now_compact() -> String {
    // std::time only: seconds since the epoch keep the filename sortable.
    format!(
        "{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    )
}

fn fnv8(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:08x}", hash & 0xffff_ffff)
}

/// Hard capture ceiling for one stream: the 10KB model-facing cap and the
/// 8KB relay threshold bound what the model sees, this bounds what the
/// agent BUFFERS — a runaway `yes` piped for the full timeout window must
/// not OOM the process. Output past the cap is dropped (the relay file
/// then holds at most the first 1MB).
pub const MAX_CAPTURE_BYTES: usize = 1024 * 1024;

async fn read_all<R: tokio::io::AsyncRead + Unpin>(mut reader: R) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 16384];
    loop {
        let n = match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        if buf.len() + n > MAX_CAPTURE_BYTES {
            let room = MAX_CAPTURE_BYTES - buf.len();
            buf.extend_from_slice(&chunk[..room]);
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
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
            session_id: None,
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
    async fn oversized_output_is_relayed_to_disk_with_head_and_tail() {
        let dir = tempdir().unwrap();
        // ~700KB of numbered lines — over the relay threshold.
        let out = run(json!({"command": "seq 1 100000"}), dir.path()).await;
        assert!(!out.is_error, "{out:?}");
        // Head and tail survive, the middle is relayed to a file.
        assert!(out.content.starts_with("1\n"), "head first lines: {out:?}");
        assert!(out.content.contains("100000"), "tail last lines: {out:?}");
        assert!(out.content.contains("relayed to"), "relay marker: {out:?}");
        assert!(
            out.content.contains("read it with the `read` tool"),
            "{out:?}"
        );
        assert!(
            out.content.len() < RELAY_THRESHOLD_BYTES,
            "summary stays lean: {} bytes",
            out.content.len()
        );
        // The relay file exists and holds the full output.
        let path_start = out.content.find("modron-relay/").expect("relay path");
        let rel: String = out.content[path_start..]
            .chars()
            .take_while(|c| *c != ' ' && *c != '\n')
            .collect();
        let file_name = rel.strip_prefix("modron-relay/").unwrap_or(&rel);
        let full =
            std::fs::read_to_string(std::env::temp_dir().join("modron-relay").join(file_name))
                .unwrap_or_default();
        assert!(full.lines().count() == 100_000, "full output relayed");
    }

    #[tokio::test]
    async fn small_output_is_not_relayed() {
        let dir = tempdir().unwrap();
        let out = run(json!({"command": "echo tiny"}), dir.path()).await;
        assert!(out.content.contains("tiny"), "{out:?}");
        assert!(!out.content.contains("relayed"), "{out:?}");
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
