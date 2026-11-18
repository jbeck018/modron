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
    /// Run once at the start of each run (observe-only; never blocks).
    pub session_start: Option<String>,
    /// Run once per model turn after its tools are dispatched (observe-only;
    /// never blocks).
    pub turn_end: Option<String>,
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
    // The cloud container's git token (handed to `modron serve` for the
    // handoff push only) and the portal host token (PLAN 10.10).
    "MODRON_GIT_TOKEN",
    "MODRON_HOST_TOKEN",
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
    secret_env_names_in(std::env::vars_os())
}

/// [`secret_env_names`] over any list of (name, value) pairs.
fn secret_env_names_in(
    vars: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> Vec<std::ffi::OsString> {
    vars.into_iter()
        .filter(|(name, value)| {
            let name = name.to_string_lossy();
            is_secret_env(&name) || is_auth_git_config(&name, &value.to_string_lossy())
        })
        .map(|(name, _)| name)
        .collect()
}

/// True for a `GIT_CONFIG_*` variable whose value sets an HTTP
/// `Authorization` header: how a git token is handed to one git call (the
/// container entrypoint and the handoff push both do this). Removed whatever
/// its name, so such a token never reaches a tool child.
pub fn is_auth_git_config(name: &str, value: &str) -> bool {
    name.to_ascii_uppercase().starts_with("GIT_CONFIG_")
        && value.to_ascii_lowercase().contains("authorization")
}

#[cfg(test)]
mod scrub_tests {
    use super::{is_auth_git_config, is_secret_env, secret_env_names_in};
    use std::ffi::OsString;

    fn pairs(list: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        list.iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect()
    }

    #[test]
    fn cloud_run_token_names_are_secret_by_name() {
        assert!(is_secret_env("MODRON_GIT_TOKEN"));
        assert!(is_secret_env("modron_host_token"));
        assert!(is_secret_env("MODRON_PUBLISH_TOKEN"));
        assert!(!is_secret_env("MODRON_RUN_ID"));
        assert!(!is_secret_env("PATH"));
    }

    #[test]
    fn git_config_carrying_an_authorization_header_is_scrubbed_whatever_its_name() {
        assert!(is_auth_git_config(
            "GIT_CONFIG_VALUE_0",
            "Authorization: Basic eC1hY2Nlc3M="
        ));
        assert!(is_auth_git_config("GIT_CONFIG_0", "x AUTHORIZATION y"));
        assert!(!is_auth_git_config("GIT_CONFIG_KEY_0", "http.extraHeader"));
        assert!(!is_auth_git_config("GIT_CONFIG_VALUE_1", "false"));
        assert!(!is_auth_git_config("HOME", "Authorization: x"));
    }

    #[test]
    fn scrub_selects_the_token_names_and_keeps_the_rest() {
        let env = pairs(&[
            ("MODRON_GIT_TOKEN", "t1"),
            ("MODRON_HOST_TOKEN", "h1"),
            ("MODRON_PUBLISH_TOKEN", "p1"),
            ("GIT_CONFIG_COUNT", "1"),
            ("GIT_CONFIG_KEY_0", "http.extraHeader"),
            ("GIT_CONFIG_VALUE_0", "Authorization: Basic eC1hY2Nlc3M="),
            ("PATH", "/usr/bin"),
            ("MODRON_RUN_ID", "run-1"),
        ]);
        let mut names: Vec<String> = secret_env_names_in(env)
            .into_iter()
            .map(|n| n.into_string().expect("ascii name"))
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "GIT_CONFIG_VALUE_0",
                "MODRON_GIT_TOKEN",
                "MODRON_HOST_TOKEN",
                "MODRON_PUBLISH_TOKEN",
            ]
        );
    }
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

/// Kills the hook's process group on drop unless disarmed.
struct HookGroup(Option<u32>);

impl HookGroup {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for HookGroup {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.0.take() {
            // Negative pid = the whole group (spawned with process_group(0)).
            unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        }
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

    spawn_hook(cmd, &child_envs, stdin_payload, timeout).await
}

/// Spawns `cmd` with the already-resolved env and stdin payload, waits up to
/// `timeout`, and maps the exit status to a [`HookOutcome`]. Shared by
/// [`run_hook`] and the lifecycle runners.
async fn spawn_hook(
    cmd: &str,
    child_envs: &[(String, String)],
    stdin_payload: Vec<u8>,
    timeout: Duration,
) -> Result<HookOutcome> {
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
    // Own process group, so a timeout or an interrupt can kill everything
    // the hook started (a backgrounded grandchild included), not just `sh`.
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().map_err(|e| Error::Tool {
        tool: "hook".into(),
        message: format!("cannot spawn hook: {e}"),
    })?;
    // Armed until the hook exits on its own; dropped armed (timeout, or
    // this future dropped on interrupt) it kills the whole group.
    let mut group = HookGroup(child.id());
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
            group.disarm();
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

/// Facts about one completed model turn, handed to the `turn_end` hook.
/// Assistant text and tool arguments are deliberately absent.
#[derive(Clone, Copy, Debug)]
pub struct TurnEndFacts<'a> {
    pub session_id: &'a str,
    pub working_dir: &'a str,
    /// 1-based turn number within this run.
    pub turn: u32,
    pub model: &'a str,
    /// Tool calls the model requested in this turn.
    pub tool_calls: u32,
    /// Provider-reported prompt tokens for the turn.
    pub input_tokens: u64,
    /// Provider-reported completion tokens for the turn.
    pub output_tokens: u64,
}

/// Logs a lifecycle hook's failure or timeout; never propagates it.
fn log_lifecycle(hook: &'static str, result: Result<HookOutcome>) {
    match result {
        Ok(outcome) if outcome.blocked => tracing::warn!(
            hook,
            message = %outcome.message,
            "lifecycle hook failed (non-blocking)"
        ),
        Ok(_) => {}
        Err(err) => tracing::warn!(hook, error = %err, "lifecycle hook failed (non-blocking)"),
    }
}

/// Runs the `session_start` hook. Env: `MODRON_EVENT=session_start`,
/// `MODRON_SESSION_ID`, `MODRON_WORKING_DIR`. Observe-only: a failure or a
/// timeout is logged and never fails the run.
pub async fn run_session_start_hook(cmd: &str, session_id: &str, working_dir: &str) {
    let envs = vec![
        ("MODRON_EVENT".to_string(), "session_start".to_string()),
        ("MODRON_SESSION_ID".to_string(), session_id.to_string()),
        ("MODRON_WORKING_DIR".to_string(), working_dir.to_string()),
    ];
    log_lifecycle("session_start", run_hook(cmd, &envs, HOOK_TIMEOUT).await);
}

/// Runs the `turn_end` hook. Env: `MODRON_EVENT=turn_end`, `MODRON_SESSION_ID`,
/// `MODRON_WORKING_DIR`, `MODRON_TURN`, `MODRON_MODEL`, `MODRON_TOOL_CALLS`.
/// Stdin is a small JSON object with the same facts plus `input_tokens` and
/// `output_tokens`. Observe-only: failures are logged, never propagated.
pub async fn run_turn_end_hook(cmd: &str, facts: TurnEndFacts<'_>) {
    let turn = facts.turn.to_string();
    let tool_calls = facts.tool_calls.to_string();
    let envs: Vec<(String, String)> = [
        ("MODRON_EVENT", "turn_end"),
        ("MODRON_SESSION_ID", facts.session_id),
        ("MODRON_WORKING_DIR", facts.working_dir),
        ("MODRON_TURN", turn.as_str()),
        ("MODRON_MODEL", facts.model),
        ("MODRON_TOOL_CALLS", tool_calls.as_str()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let stdin = serde_json::json!({
        "event": "turn_end",
        "session_id": facts.session_id,
        "working_dir": facts.working_dir,
        "turn": facts.turn,
        "model": facts.model,
        "tool_calls": facts.tool_calls,
        "input_tokens": facts.input_tokens,
        "output_tokens": facts.output_tokens,
    })
    .to_string()
    .into_bytes();
    log_lifecycle(
        "turn_end",
        spawn_hook(cmd, &envs, stdin, HOOK_TIMEOUT).await,
    );
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
    #[cfg(unix)]
    #[tokio::test]
    async fn timed_out_hook_takes_its_background_children_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("grandchild-ran");
        // The grandchild outlives `sh` unless the whole group is killed.
        let cmd = format!("(sleep 1; touch {}) & sleep 30", marker.display());
        let outcome = run_hook(&cmd, &[], Duration::from_millis(150))
            .await
            .unwrap();
        assert!(outcome.blocked, "{outcome:?}");
        tokio::time::sleep(Duration::from_millis(1400)).await;
        assert!(
            !marker.exists(),
            "the backgrounded child must be killed too"
        );
    }

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

    #[tokio::test]
    async fn session_start_runner_sets_its_env_and_never_panics_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let seen = dir.path().join("start.txt");
        let cmd = format!(
            "printf '%s|%s|%s' \"$MODRON_EVENT\" \"$MODRON_SESSION_ID\" \"$MODRON_WORKING_DIR\" > {}",
            seen.display()
        );
        run_session_start_hook(&cmd, "sess-9", "/work/dir").await;
        assert_eq!(
            std::fs::read_to_string(&seen).unwrap(),
            "session_start|sess-9|/work/dir"
        );
        // A failing hook is logged and swallowed (returns unit, no panic).
        run_session_start_hook("exit 7", "sess-9", "/work/dir").await;
    }

    #[tokio::test]
    async fn turn_end_runner_sets_env_and_streams_json_on_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let env_out = dir.path().join("env.txt");
        let stdin_out = dir.path().join("stdin.json");
        let cmd = format!(
            "printf '%s|%s|%s|%s|%s|%s' \"$MODRON_EVENT\" \"$MODRON_SESSION_ID\" \"$MODRON_TURN\" \"$MODRON_MODEL\" \"$MODRON_TOOL_CALLS\" \"$MODRON_WORKING_DIR\" > {env}; cat > {stdin}",
            env = env_out.display(),
            stdin = stdin_out.display()
        );
        run_turn_end_hook(
            &cmd,
            TurnEndFacts {
                session_id: "sess-7",
                working_dir: "/w",
                turn: 3,
                model: "m-1",
                tool_calls: 2,
                input_tokens: 100,
                output_tokens: 40,
            },
        )
        .await;
        assert_eq!(
            std::fs::read_to_string(&env_out).unwrap(),
            "turn_end|sess-7|3|m-1|2|/w"
        );
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&stdin_out).unwrap()).unwrap();
        assert_eq!(json["event"], "turn_end");
        assert_eq!(json["turn"], 3);
        assert_eq!(json["tool_calls"], 2);
        assert_eq!(json["input_tokens"], 100);
        assert_eq!(json["output_tokens"], 40);
        assert!(json.get("text").is_none() && json.get("args").is_none());
    }

    #[tokio::test]
    async fn turn_end_hook_env_does_not_leak_provider_secret() {
        // Unique name: no other test reads or sets it.
        std::env::set_var("MODRON_TEST_TURN_API_KEY", "sekrit-turn");
        let dir = tempfile::tempdir().unwrap();
        let seen = dir.path().join("seen.txt");
        let cmd = format!(
            "printf '%s' \"${{MODRON_TEST_TURN_API_KEY-unset}}\" > {}",
            seen.display()
        );
        run_turn_end_hook(
            &cmd,
            TurnEndFacts {
                session_id: "s",
                working_dir: "/w",
                turn: 1,
                model: "m",
                tool_calls: 0,
                input_tokens: 0,
                output_tokens: 0,
            },
        )
        .await;
        std::env::remove_var("MODRON_TEST_TURN_API_KEY");
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
