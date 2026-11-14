//! `bash` tool (LEDGER T1) — shell execution with output caps and timeouts.

use crate::common::{arg_str, internal_tool_error, opt_u64, tool_err};
use crate::reduce::reduce_output;
use crate::validate::validate_args;
use async_trait::async_trait;
use modron_core::hooks::scrub_tokio_command;
use modron_core::{Result, Tool, ToolCtx, ToolOutcome, ToolSpec};
use serde_json::{json, Value};
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::task::JoinHandle;

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

        // Sandbox routing (A2): under Seatbelt (macOS) or Bubblewrap (Linux)
        // the whole shell runs inside the OS sandbox (network denied, writes
        // fenced to the workspace + temp dirs). The sandboxed path cannot
        // stream-pipe the same way, so it uses one buffered capture; caps and
        // exit-code reporting are identical. Timeout handling is shared below.
        if matches!(
            ctx.sandbox,
            modron_core::SandboxPolicy::Seatbelt | modron_core::SandboxPolicy::Bubblewrap
        ) {
            let started = std::time::Instant::now();
            return match tokio::time::timeout(
                Duration::from_millis(timeout_ms),
                modron_core::sandbox::run_sandboxed(ctx.sandbox, command, &ctx.working_dir),
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
        // D9: provider secrets never reach the child environment. The
        // sandboxed path above is not scrubbed yet (sandbox.rs is outside D3).
        scrub_tokio_command(&mut cmd);
        cmd.args(shell_args(command))
            .current_dir(&ctx.working_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Own process group: on timeout or interrupt the WHOLE tree
        // (grandchildren like `cargo`, pipelines, `cmd &`) is signaled, not
        // just the `sh`.
        #[cfg(unix)]
        cmd.process_group(0);

        let mut child = cmd
            .spawn()
            .map_err(|e| internal_tool_error(NAME, format!("failed to spawn `{command}`: {e}")))?;
        // Armed until the child is reaped normally. If this future is dropped
        // mid-command (user interrupt), the guard kills the group on the way out.
        let mut group = GroupGuard::armed(&child);
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| internal_tool_error(NAME, "stdout was not captured"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| internal_tool_error(NAME, "stderr was not captured"))?;

        // Readers run concurrently with `wait` so full pipes never deadlock.
        let out_reader = spawn_capture(stdout);
        let err_reader = spawn_capture(stderr);

        // Bind first: the timeout future borrows `child`; the match must not.
        let waited = tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait()).await;
        match waited {
            Ok(Ok(status)) => {
                group.disarm();
                let (out, err) = settle_readers(out_reader, err_reader).await;
                let truncated = out.truncated || err.truncated;
                let text = merged_output(&out, &err);
                let code_line = match status.code() {
                    Some(code) => format!("exit code: {code}"),
                    None => "exit code: terminated by signal".to_string(),
                };
                let body = with_truncation_marker(
                    reduce_output(relay_oversized(&text), body_budget(truncated)),
                    truncated,
                );
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
                group.disarm();
                let (out, err) = settle_readers(out_reader, err_reader).await;
                let truncated = out.truncated || err.truncated;
                let text = merged_output(&out, &err);
                let body =
                    with_truncation_marker(reduce_output(text, body_budget(truncated)), truncated);
                Ok(ToolOutcome::err(format!(
                    "{body}\ncommand timed out after {timeout_ms} ms and was killed; partial output above"
                )))
            }
        }
    }
}

/// Kills the child's process group when dropped, unless disarmed. The group
/// is only signaled while the child is unreaped, so its pgid cannot have been
/// recycled. A bare `kill_on_drop` would leave grandchildren running.
struct GroupGuard {
    pgid: Option<i32>,
}

impl GroupGuard {
    fn armed(child: &tokio::process::Child) -> Self {
        Self {
            pgid: child.id().map(|pid| pid as i32),
        }
    }

    fn disarm(&mut self) {
        self.pgid = None;
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(pgid) = self.pgid.take() {
            kill_group(pgid);
        }
    }
}

/// SIGKILL every process in the group `pgid` (the spawned shell's pid).
#[cfg(unix)]
fn kill_group(pgid: i32) {
    // Negative pid = the whole group (we spawned with group 0).
    unsafe { libc::kill(-pgid, libc::SIGKILL) };
}
#[cfg(not(unix))]
fn kill_group(_pgid: i32) {}

/// Kill the spawned shell AND its process group (grandchildren included),
/// then reap. `child.kill()` alone would leave `cargo`, pipelines, and
/// background spawns running as orphans.
async fn kill_child_tree(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        kill_group(pid as i32);
    }
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

/// Model-facing marker appended when a stream hit `MAX_CAPTURE_BYTES`.
pub const TRUNCATED_MARKER: &str = "[output truncated at 1 MB]";

/// How long the pipe readers may keep draining after the child has exited. A
/// backgrounded grandchild can hold the pipe open indefinitely; past this
/// window the readers are abandoned and whatever arrived is returned.
const READER_GRACE: Duration = Duration::from_secs(2);

/// One drained pipe. Shared with the reader task so the bytes survive if the
/// task is abandoned during the grace window.
#[derive(Default)]
struct Captured {
    bytes: Vec<u8>,
    truncated: bool,
}

type SharedCapture = Arc<Mutex<Captured>>;

fn lock(shared: &SharedCapture) -> MutexGuard<'_, Captured> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Starts draining `reader` in the background into a capped buffer.
fn spawn_capture<R>(reader: R) -> (SharedCapture, JoinHandle<()>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let shared = SharedCapture::default();
    let task = tokio::spawn(drain_capped(reader, Arc::clone(&shared)));
    (shared, task)
}

/// Reads to EOF. Past `MAX_CAPTURE_BYTES` the bytes are discarded but the pipe
/// is still drained, so the child keeps running to its real exit status
/// instead of dying of SIGPIPE when we stop reading.
async fn drain_capped<R: AsyncRead + Unpin>(mut reader: R, shared: SharedCapture) {
    let mut chunk = [0u8; 16384];
    loop {
        let n = match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let mut cap = lock(&shared);
        let room = MAX_CAPTURE_BYTES.saturating_sub(cap.bytes.len());
        if n > room {
            cap.truncated = true;
        }
        cap.bytes.extend_from_slice(&chunk[..n.min(room)]);
    }
}

/// Waits up to `READER_GRACE` for both readers to reach EOF, then aborts any
/// still reading and returns what each captured.
async fn settle_readers(
    out: (SharedCapture, JoinHandle<()>),
    err: (SharedCapture, JoinHandle<()>),
) -> (Captured, Captured) {
    let (out_shared, mut out_task) = out;
    let (err_shared, mut err_task) = err;
    let _ = tokio::time::timeout(READER_GRACE, async {
        let _ = (&mut out_task).await;
        let _ = (&mut err_task).await;
    })
    .await;
    out_task.abort();
    err_task.abort();
    (take_captured(&out_shared), take_captured(&err_shared))
}

fn take_captured(shared: &SharedCapture) -> Captured {
    std::mem::take(&mut *lock(shared))
}

/// Merges captured streams: stdout, then stderr on its own line if non-empty.
fn merged_output(stdout: &Captured, stderr: &Captured) -> String {
    let mut text = String::from_utf8_lossy(&stdout.bytes).into_owned();
    let err_text = String::from_utf8_lossy(&stderr.bytes);
    if !err_text.trim().is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&err_text);
    }
    text
}

/// Byte budget for the reduced body, leaving room for the marker when
/// output was truncated so the final body stays within `MAX_BYTES`.
fn body_budget(truncated: bool) -> usize {
    if truncated {
        MAX_BYTES - TRUNCATED_MARKER.len() - 1
    } else {
        MAX_BYTES
    }
}

/// Appends the truncation marker as the last line of the body.
fn with_truncation_marker(body: String, truncated: bool) -> String {
    match (truncated, body.is_empty()) {
        (false, _) => body,
        (true, true) => TRUNCATED_MARKER.to_string(),
        (true, false) => format!("{body}\n{TRUNCATED_MARKER}"),
    }
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

    /// Reads a pid written by the shell under test.
    #[cfg(unix)]
    fn read_pid(path: &std::path::Path) -> Option<i32> {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| s.trim().parse().ok())
    }

    /// True while the pid names a live process (zombies count as gone).
    #[cfg(unix)]
    fn is_alive(pid: i32) -> bool {
        if unsafe { libc::kill(pid, 0) } != 0 {
            return false;
        }
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => !stat
                .rsplit(')')
                .next()
                .unwrap_or("")
                .trim_start()
                .starts_with('Z'),
            Err(_) => true,
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn backgrounded_child_holding_the_pipe_does_not_hang_the_tool() {
        let dir = tempdir().unwrap();
        let bg_pid = dir.path().join("bg.pid");
        let command = format!("sleep 300 & echo $! > {}; echo started", bg_pid.display());
        let started = std::time::Instant::now();
        let out = tokio::time::timeout(
            Duration::from_secs(20),
            run(json!({"command": command, "timeout_ms": 10000}), dir.path()),
        )
        .await;
        if let Some(pid) = read_pid(&bg_pid) {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        let out = out.expect("tool hung on a backgrounded child that holds stdout");
        assert!(!out.is_error, "{out:?}");
        assert!(out.content.contains("started"), "{out:?}");
        assert!(out.content.ends_with("exit code: 0"), "{out:?}");
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "took {:?}",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_the_tool_future_kills_the_whole_process_group() {
        let dir = tempdir().unwrap();
        let sh_pid = dir.path().join("sh.pid");
        let child_pid = dir.path().join("child.pid");
        let command = format!(
            "echo $$ > {}; sleep 300 & echo $! > {}; wait",
            sh_pid.display(),
            child_pid.display()
        );
        let ctx = ToolCtx {
            engine: None,
            sandbox: modron_core::SandboxPolicy::Off,
            working_dir: dir.path().to_path_buf(),
            session_id: None,
        };
        // The caller drops the tool future while the command is still
        // running, as an interrupt does.
        let fut = BashTool.execute(json!({"command": command, "timeout_ms": 60000}), &ctx);
        let _ = tokio::time::timeout(Duration::from_millis(1500), fut).await;

        let pids: Vec<i32> = [read_pid(&sh_pid), read_pid(&child_pid)]
            .into_iter()
            .flatten()
            .collect();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline && pids.iter().any(|p| is_alive(*p)) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let alive: Vec<i32> = pids.iter().copied().filter(|p| is_alive(*p)).collect();
        for pid in &alive {
            unsafe { libc::kill(*pid, libc::SIGKILL) };
        }
        assert_eq!(pids.len(), 2, "shell and background pids were recorded");
        assert!(
            alive.is_empty(),
            "processes survived the interrupt: {alive:?}"
        );
    }

    #[tokio::test]
    async fn truncated_output_reports_marker_with_real_exit_status() {
        let dir = tempdir().unwrap();
        let out = run(json!({"command": "yes | head -c 5000000"}), dir.path()).await;
        assert!(!out.is_error, "{out:?}");
        assert!(
            out.content.contains("[output truncated at 1 MB]"),
            "{out:?}"
        );
        assert!(out.content.ends_with("exit code: 0"), "{out:?}");
        assert!(!out.content.contains("terminated by signal"), "{out:?}");
    }

    #[tokio::test]
    async fn truncated_output_keeps_nonzero_exit_status() {
        let dir = tempdir().unwrap();
        let out = run(
            json!({"command": "yes | head -c 5000000; exit 3"}),
            dir.path(),
        )
        .await;
        assert!(out.is_error, "{out:?}");
        assert!(
            out.content.contains("[output truncated at 1 MB]"),
            "{out:?}"
        );
        assert!(out.content.ends_with("exit code: 3"), "{out:?}");
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

    #[tokio::test]
    async fn child_env_never_carries_provider_secrets() {
        // Unique name: no other test reads or sets it.
        std::env::set_var("MODRON_TEST_FAKE_API_KEY", "sekrit");
        let dir = tempdir().unwrap();
        // `env` filtered to the two names under test: the full environment
        // is long and the tool's output relay may cut the middle, which
        // would hide a leak. The filter keeps the check deterministic.
        let out = run(
            json!({"command": "env | grep -E '^(PATH|MODRON_TEST_FAKE_API_KEY)='"}),
            dir.path(),
        )
        .await;
        std::env::remove_var("MODRON_TEST_FAKE_API_KEY");
        assert!(out.content.contains("PATH="), "{out:?}");
        assert!(!out.content.contains("sekrit"), "{out:?}");
        assert!(!out.content.contains("MODRON_TEST_FAKE_API_KEY"), "{out:?}");
    }
}
