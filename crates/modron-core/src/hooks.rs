//! Lifecycle hooks (Phase-1b H1): user-configured shell commands run before
//! and after every tool call. A non-zero-exit or timed-out `pre_tool` hook
//! vetoes the tool call — its stderr tail becomes the tool error the model
//! sees — so hooks can enforce policy (formatting, protected paths, audits)
//! without touching harness code. `post_tool` failures are logged, never
//! blocking.
//!
//! Hook contract (env vars, always set): `MODRON_TOOL_NAME`,
//! `MODRON_TOOL_ARGS` (compact JSON), `MODRON_SESSION_ID`, and for
//! `post_tool` also `MODRON_TOOL_EXIT_CODE` ("0"/"1" — the tool's
//! `ToolOutcome::is_error`, not the shell's).
//!
//! Blocking-in-async note: like the JSONL store, hook processes run on the
//! async executor via `tokio::process` with an explicit timeout; acceptable
//! for the one-shot CLI.

use crate::{Error, Result};
use std::process::Stdio;
use std::time::Duration;

/// Default wall-clock budget for one hook invocation.
pub const HOOK_TIMEOUT: Duration = Duration::from_secs(10);

/// User-configured hook commands (wired from config in a later tranche;
/// today they arrive via `EngineConfig` in tests and the future config file).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HookConfig {
    /// Run before every tool call; non-zero exit or timeout vetoes the call.
    pub pre_tool: Option<String>,
    /// Run after every tool call completes; failures never block.
    pub post_tool: Option<String>,
}

/// What a hook decided. `blocked` is only meaningful for `pre_tool`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookOutcome {
    pub blocked: bool,
    /// Human-readable reason (stderr tail on veto; empty on pass).
    pub message: String,
}

/// Run `cmd` through `sh -c` with the given env vars. Non-zero exit or a
/// timeout counts as a veto (for pre-tool hooks; post-tool callers ignore
/// `blocked` and just log).
pub async fn run_hook(
    cmd: &str,
    envs: &[(String, String)],
    timeout: Duration,
) -> Result<HookOutcome> {
    let mut command = tokio::process::Command::new("sh");
    command
        .arg("-c")
        .arg(cmd)
        .envs(envs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().map_err(|e| Error::Tool {
        tool: "hook".into(),
        message: format!("cannot spawn hook: {e}"),
    })?;

    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => {
            return Err(Error::Tool {
                tool: "hook".into(),
                message: format!("hook failed to run: {e}"),
            });
        }
        Err(_) => {
            return Ok(HookOutcome {
                blocked: true,
                message: format!("hook timed out after {}s", timeout.as_secs()),
            });
        }
    };

    let stderr_tail = tail(String::from_utf8_lossy(&output.stderr).trim(), 400);
    if output.status.success() {
        Ok(HookOutcome {
            blocked: false,
            message: String::new(),
        })
    } else {
        Ok(HookOutcome {
            blocked: true,
            message: format!(
                "hook exited with {} — {}",
                output.status.code().unwrap_or(-1),
                stderr_tail
            ),
        })
    }
}

/// Last `max` characters of `s` (char-boundary safe) — for surfacing hook
/// stderr without flooding the model's context.
fn tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let start = s
        .char_indices()
        .rev()
        .nth(max.saturating_sub(1))
        .map(|(i, _)| i)
        .unwrap_or(0);
    format!("…{}", &s[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn passing_hook_is_not_blocking() {
        let out = run_hook("exit 0", &[], Duration::from_secs(5))
            .await
            .unwrap();
        assert!(!out.blocked);
    }

    #[tokio::test]
    async fn failing_hook_vetoes_with_stderr_tail() {
        let out = run_hook(
            "echo protected-file >&2; exit 1",
            &[],
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert!(out.blocked);
        assert!(
            out.message.contains("protected-file"),
            "got: {}",
            out.message
        );
        assert!(out.message.contains("exited with 1"));
    }

    #[tokio::test]
    async fn timed_out_hook_vetoes() {
        let out = run_hook("sleep 5", &[], Duration::from_millis(150))
            .await
            .unwrap();
        assert!(out.blocked);
        assert!(out.message.contains("timed out"));
    }

    #[tokio::test]
    async fn env_vars_reach_the_hook() {
        let dir = tempfile::tempdir().unwrap();
        let env_out = dir.path().join("env.txt");
        let script = format!(
            "printf '%s|%s|%s' \"$MODRON_TOOL_NAME\" \"$MODRON_SESSION_ID\" \"$MODRON_TOOL_ARGS\" > {}",
            env_out.display()
        );
        let envs = vec![
            ("MODRON_TOOL_NAME".to_string(), "write".to_string()),
            ("MODRON_SESSION_ID".to_string(), "sess-123".to_string()),
            (
                "MODRON_TOOL_ARGS".to_string(),
                "{\"path\":\"a.txt\"}".to_string(),
            ),
        ];
        let out = run_hook(&script, &envs, Duration::from_secs(5))
            .await
            .unwrap();
        assert!(!out.blocked);
        let captured = std::fs::read_to_string(&env_out).unwrap();
        assert_eq!(captured, "write|sess-123|{\"path\":\"a.txt\"}");
    }

    #[test]
    fn tail_is_char_boundary_safe() {
        assert_eq!(tail("short", 10), "short");
        let long = "x".repeat(500);
        // "…" is 3 bytes; 400 ASCII x's follow → 403 bytes, 401 chars.
        assert_eq!(tail(&long, 400).len(), 403);
        assert_eq!(tail(&long, 400).chars().count(), 401);
        let multi = "é".repeat(300);
        assert!(tail(&multi, 400).starts_with('…'));
        assert_eq!(tail(&multi, 400).chars().count(), 301);
    }
}
