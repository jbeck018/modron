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
pub async fn run_hook(
    cmd: &str,
    envs: &[(String, String)],
    timeout: Duration,
) -> Result<HookOutcome> {
    let mut command = tokio::process::Command::new("sh");
    scrub_tokio_command(&mut command);
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
