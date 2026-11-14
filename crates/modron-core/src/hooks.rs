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
//! Large arguments: Linux rejects a single environment string over 128 KiB
//! (E2BIG), which would make every large `write` fail its spawn and, for a
//! `pre_tool` hook, be vetoed. So `MODRON_TOOL_ARGS` is capped at
//! [`MAX_ENV_ARGS_BYTES`] (truncated on a char boundary, suffixed with
//! ` ...[truncated]`) and `MODRON_TOOL_ARGS_TRUNCATED=1` is set when that
//! happens. The FULL, untruncated arguments JSON is always written to the
//! hook's stdin, which is then closed. Hooks that need complete arguments
//! must read stdin.
//!
//! Timeouts: a hook that exceeds its timeout, or whose future is dropped
//! (e.g. on interrupt), is killed (`kill_on_drop` plus an explicit kill on
//! the timeout path); it never keeps running in the background.
//!
//! Blocking-in-async note: like the JSONL store, hook processes run on the
//! async executor via `tokio::process` with an explicit timeout; acceptable
//! for the one-shot CLI.

use crate::{Error, Result};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Default wall-clock budget for one hook invocation.
pub const HOOK_TIMEOUT: Duration = Duration::from_secs(10);

/// Largest `MODRON_TOOL_ARGS` value placed in the environment, in bytes.
/// Above this the value is truncated and the full JSON goes on stdin.
pub const MAX_ENV_ARGS_BYTES: usize = 32 * 1024;

/// Env var carrying the (possibly truncated) tool arguments JSON.
const TOOL_ARGS_VAR: &str = "MODRON_TOOL_ARGS";
/// Env var set to "1" when `MODRON_TOOL_ARGS` was truncated.
const TOOL_ARGS_TRUNCATED_VAR: &str = "MODRON_TOOL_ARGS_TRUNCATED";
/// Suffix appended to a truncated env value.
const TRUNCATION_SUFFIX: &str = " ...[truncated]";

/// `Some(capped)` when `value` exceeds `max` bytes: the prefix cut on a char
/// boundary plus [`TRUNCATION_SUFFIX`]. `None` when it fits.
fn cap_env_value(value: &str, max: usize) -> Option<String> {
    if value.len() <= max {
        return None;
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    Some(format!("{}{TRUNCATION_SUFFIX}", &value[..end]))
}

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

/// Exact variable names that are never treated as secrets (compared
/// case-insensitively). Decision D9.
const SCRUB_EXEMPT: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "SHELL",
    "TERM",
    "LANG",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "SSH_AUTH_SOCK",
];

/// Prefix of the Modron hook/tool contract variables; never treated as
/// secrets, whatever follows the prefix.
const TOOL_CONTRACT_PREFIX: &str = "MODRON_TOOL_";

/// Name suffixes that mark a model-provider credential (matched
/// case-insensitively). Deliberately narrow: generic `_TOKEN` / `_SECRET` /
/// `_PASSWORD` names are NOT scrubbed, because the agent's shell needs the
/// user's own tooling credentials (`GITHUB_TOKEN` for gh and git,
/// `CARGO_REGISTRY_TOKEN`, database passwords for tests) to do its job.
const SECRET_SUFFIXES: &[&str] = &["_API_KEY"];

/// Modron's own non-`_API_KEY` credentials, by exact name.
const SECRET_EXACT: &[&str] = &[
    "GITHUB_MODELS_TOKEN",
    "MODRON_GITHUB_TOKEN",
    "MODRON_DEVICE_TOKEN",
    "MODRON_PUBLISH_TOKEN",
];

/// Name prefixes that mark a provider credential or config (matched
/// case-insensitively).
const SECRET_PREFIXES: &[&str] = &["ANTHROPIC_", "OPENAI_", "ZAI_"];

/// Decides whether the environment variable `name` must not be inherited by
/// a child process that Modron spawns for a tool or hook (decision D9).
/// Matching is case-insensitive.
pub fn is_secret_env(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    if upper.starts_with(TOOL_CONTRACT_PREFIX) || SCRUB_EXEMPT.contains(&upper.as_str()) {
        return false;
    }
    SECRET_EXACT.contains(&upper.as_str())
        || SECRET_SUFFIXES.iter().any(|s| upper.ends_with(s))
        || SECRET_PREFIXES.iter().any(|p| upper.starts_with(p))
}

/// Names of the variables in the current process environment that
/// [`is_secret_env`] matches. Names that are not UTF-8 are judged lossily and
/// returned as the original `OsString`, so they can still be removed.
fn secret_env_names() -> Vec<std::ffi::OsString> {
    std::env::vars_os()
        .filter(|(name, _)| is_secret_env(&name.to_string_lossy()))
        .map(|(name, _)| name)
        .collect()
}

/// Removes every secret variable (see [`is_secret_env`]) from the child
/// environment of a `std::process::Command`. Call it before other env
/// configuration: a later explicit `env`/`envs` for the same name still wins.
pub fn scrub_std_command(cmd: &mut std::process::Command) {
    for name in secret_env_names() {
        cmd.env_remove(name);
    }
}

/// `tokio::process::Command` counterpart of [`scrub_std_command`].
pub fn scrub_tokio_command(cmd: &mut tokio::process::Command) {
    for name in secret_env_names() {
        cmd.env_remove(name);
    }
}

/// Run `cmd` through `sh -c` with the given env vars. Non-zero exit or a
/// timeout counts as a veto (for pre-tool hooks; post-tool callers ignore
/// `blocked` and just log).
///
/// `MODRON_TOOL_ARGS` is capped in the environment (see the module docs); the
/// full value is always written to the hook's stdin, which is then closed.
/// A timed-out hook is killed, as is one whose future is dropped.
pub async fn run_hook(
    cmd: &str,
    envs: &[(String, String)],
    timeout: Duration,
) -> Result<HookOutcome> {
    let mut stdin_payload: Vec<u8> = Vec::new();
    let mut child_envs: Vec<(String, String)> = Vec::with_capacity(envs.len() + 1);
    for (name, value) in envs {
        if name == TOOL_ARGS_VAR {
            stdin_payload = value.as_bytes().to_vec();
            match cap_env_value(value, MAX_ENV_ARGS_BYTES) {
                Some(capped) => {
                    child_envs.push((name.clone(), capped));
                    child_envs.push((TOOL_ARGS_TRUNCATED_VAR.to_string(), "1".to_string()));
                }
                None => child_envs.push((name.clone(), value.clone())),
            }
        } else {
            child_envs.push((name.clone(), value.clone()));
        }
    }

    let mut command = tokio::process::Command::new("sh");
    scrub_tokio_command(&mut command);
    command
        .arg("-c")
        .arg(cmd)
        .envs(child_envs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|e| Error::Tool {
        tool: "hook".into(),
        message: format!("cannot spawn hook: {e}"),
    })?;
    let stdin = child.stdin.take();
    let mut stderr = child.stderr.take();

    let run = async {
        let feed = async {
            if let Some(mut stdin) = stdin {
                // A hook that exits without reading stdin breaks the pipe;
                // that is not an error for the hook's verdict.
                let _ = stdin.write_all(&stdin_payload).await;
                let _ = stdin.shutdown().await;
            }
        };
        let read_stderr = async {
            let mut buf = Vec::new();
            if let Some(stderr) = stderr.as_mut() {
                let _ = stderr.read_to_end(&mut buf).await;
            }
            buf
        };
        let ((), stderr_bytes, status) = tokio::join!(feed, read_stderr, child.wait());
        (stderr_bytes, status)
    };

    let (stderr_bytes, status) = match tokio::time::timeout(timeout, run).await {
        Ok((stderr_bytes, status)) => {
            let status = status.map_err(|e| Error::Tool {
                tool: "hook".into(),
                message: format!("hook failed to run: {e}"),
            })?;
            (stderr_bytes, status)
        }
        Err(_) => {
            // The run future is gone; make sure the hook process is too.
            let _ = child.kill().await;
            return Ok(HookOutcome {
                blocked: true,
                message: format!("hook timed out after {}s", timeout.as_secs()),
            });
        }
    };

    let stderr_tail = tail(String::from_utf8_lossy(&stderr_bytes).trim(), 400);
    if status.success() {
        Ok(HookOutcome {
            blocked: false,
            message: String::new(),
        })
    } else {
        Ok(HookOutcome {
            blocked: true,
            message: format!(
                "hook exited with {} — {}",
                status.code().unwrap_or(-1),
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
    async fn timed_out_hook_is_killed_and_its_marker_never_appears() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("late.txt");
        let script = format!("sleep 1; touch {}", marker.display());
        let out = run_hook(&script, &[], Duration::from_millis(100))
            .await
            .unwrap();
        assert!(out.blocked);
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(!marker.exists(), "timed-out hook kept running");
    }

    #[tokio::test]
    async fn dropped_hook_future_kills_the_hook() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("late.txt");
        let script = format!("sleep 1; touch {}", marker.display());
        // Simulate an interrupt: the outer timeout drops the run_hook future.
        let _ = tokio::time::timeout(
            Duration::from_millis(100),
            run_hook(&script, &[], Duration::from_secs(30)),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(!marker.exists(), "dropped hook kept running");
    }

    #[tokio::test]
    async fn oversized_tool_args_are_capped_in_env_and_full_on_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let stdin_out = dir.path().join("stdin.json");
        let flag = dir.path().join("flag.txt");
        let args = format!("{{\"content\":\"{}\"}}", "a".repeat(300 * 1024));
        let envs = vec![("MODRON_TOOL_ARGS".to_string(), args.clone())];
        // Env value must be capped (well under the 128 KiB single-string limit).
        let script = format!(
            "cat > {stdin}; printf '%s' \"$MODRON_TOOL_ARGS_TRUNCATED\" > {flag}; \
             test \"${{#MODRON_TOOL_ARGS}}\" -le 40000",
            stdin = stdin_out.display(),
            flag = flag.display(),
        );
        let out = run_hook(&script, &envs, Duration::from_secs(10))
            .await
            .unwrap();
        assert!(!out.blocked, "hook vetoed: {}", out.message);
        assert_eq!(std::fs::read_to_string(&stdin_out).unwrap(), args);
        assert_eq!(std::fs::read_to_string(&flag).unwrap(), "1");
    }

    #[test]
    fn tool_args_cap_is_char_boundary_safe() {
        let small = "{\"a\":1}";
        assert_eq!(cap_env_value(small, MAX_ENV_ARGS_BYTES), None);
        let multi = "é".repeat(MAX_ENV_ARGS_BYTES);
        let capped = cap_env_value(&multi, MAX_ENV_ARGS_BYTES).unwrap();
        assert!(capped.ends_with(" ...[truncated]"));
        let body = capped.strip_suffix(" ...[truncated]").unwrap();
        assert!(body.len() <= MAX_ENV_ARGS_BYTES);
        assert!(body.chars().all(|c| c == 'é'));
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
    fn secret_env_predicate_table() {
        let secret = [
            "ANTHROPIC_API_KEY",
            "anthropic_api_key",
            "OPENAI_API_KEY",
            "ZAI_API_KEY",
            "ANTHROPIC_BASE_URL",
            "OPENAI_BASE_URL",
            "zai_model",
            "Stripe_Api_Key",
            "GITHUB_MODELS_TOKEN",
            "modron_device_token",
        ];
        for name in secret {
            assert!(is_secret_env(name), "{name} should be scrubbed");
        }
        let kept = [
            "GITHUB_TOKEN",
            "CARGO_REGISTRY_TOKEN",
            "my_secret",
            "DB_PASSWORD",
            "PATH",
            "HOME",
            "USER",
            "SHELL",
            "TERM",
            "LANG",
            "CARGO_HOME",
            "RUSTUP_HOME",
            "SSH_AUTH_SOCK",
            "path",
            "MODRON_TOOL_NAME",
            "MODRON_TOOL_ARGS",
            "MODRON_TOOL_API_KEY",
            "MODRON_SESSION_ID",
            "TOKENIZER_DIR",
            "MY_TOKEN_DIR",
            "SECRETARY",
            "",
        ];
        for name in kept {
            assert!(!is_secret_env(name), "{name} should be kept");
        }
    }

    #[test]
    fn scrub_std_command_removes_secret_vars_only() {
        // Unique name: no other test reads or sets it.
        std::env::set_var("MODRON_TEST_STD_API_KEY", "sekrit-std");
        let mut cmd = std::process::Command::new("sh");
        scrub_std_command(&mut cmd);
        let out = cmd
            .arg("-c")
            .arg("printf '%s|%s' \"${MODRON_TEST_STD_API_KEY-unset}\" \"${PATH:+path}\"")
            .output()
            .unwrap();
        std::env::remove_var("MODRON_TEST_STD_API_KEY");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "unset|path");
    }

    #[tokio::test]
    async fn scrub_tokio_command_removes_secret_vars_only() {
        // Unique name: no other test reads or sets it.
        std::env::set_var("MODRON_TEST_TOKIO_API_KEY", "sekrit-tokio");
        let mut cmd = tokio::process::Command::new("sh");
        scrub_tokio_command(&mut cmd);
        let out = cmd
            .arg("-c")
            .arg("printf '%s|%s' \"${MODRON_TEST_TOKIO_API_KEY-unset}\" \"${PATH:+path}\"")
            .output()
            .await
            .unwrap();
        std::env::remove_var("MODRON_TEST_TOKIO_API_KEY");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "unset|path");
    }

    #[tokio::test]
    async fn hook_env_does_not_leak_provider_secret() {
        // Unique name: no other test reads or sets it.
        std::env::set_var("MODRON_TEST_HOOK_API_KEY", "sekrit-hook");
        let dir = tempfile::tempdir().unwrap();
        let seen = dir.path().join("seen.txt");
        let script = format!(
            "printf '%s' \"${{MODRON_TEST_HOOK_API_KEY-unset}}\" > {}",
            seen.display()
        );
        let out = run_hook(&script, &[], Duration::from_secs(5))
            .await
            .unwrap();
        std::env::remove_var("MODRON_TEST_HOOK_API_KEY");
        assert!(!out.blocked, "{}", out.message);
        assert_eq!(std::fs::read_to_string(&seen).unwrap(), "unset");
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
