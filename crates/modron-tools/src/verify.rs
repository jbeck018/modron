//! The `verify` tool (Phase-2a D3): runs the project's own configured check
//! command (tests, lints, builds) so the agent can catch breakage right
//! after edits — the six-responsibilities framework's most-neglected pillar
//! (verification), and the cheapest quality lever per token: a failing
//! `cargo test` line beats a model guessing.
//!
//! The command comes from `[verify] command` in config.toml. The tool only
//! exists when a command is configured, so the prompt budget stays lean
//! otherwise. It is treated as a mutating tool (approval-gated) because test
//! suites can write files and hit the network.

use crate::common::{internal_tool_error, tool_err};
use crate::reduce::reduce_output;
use crate::validate::validate_args;
use modron_core::{Tool, ToolCtx, ToolOutcome, ToolSpec};
use std::process::Stdio;
use std::time::Duration;

const NAME: &str = "verify";
const MAX_BYTES: usize = 10 * 1024;
const TIMEOUT: Duration = Duration::from_secs(120);

pub struct VerifyTool {
    command: String,
}

impl VerifyTool {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
        }
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: NAME.to_string(),
            description: "Run the project's verify command (tests/checks configured for this \
                          repo). Call it after making edits and fix failures before finishing."
                .to_string(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }
    }
}

#[async_trait::async_trait]
impl Tool for VerifyTool {
    fn spec(&self) -> ToolSpec {
        VerifyTool::spec(self)
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx,
    ) -> modron_core::Result<ToolOutcome> {
        if let Err(message) = validate_args(&self.spec(), &args) {
            return Ok(tool_err(
                NAME,
                format!("invalid arguments: {message}"),
                MAX_BYTES,
            ));
        }

        let output = tokio::time::timeout(
            TIMEOUT,
            tokio::process::Command::new("sh")
                .arg("-c")
                .arg(&self.command)
                .current_dir(&ctx.working_dir)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                // The verify command can write files; a dropped timeout
                // future must kill it, not detach it to keep running.
                .kill_on_drop(true)
                .output(),
        )
        .await;

        match output {
            Err(_) => Ok(ToolOutcome::err(format!(
                "verify timed out after {}s (command: {})",
                TIMEOUT.as_secs(),
                self.command
            ))),
            Ok(Err(e)) => Err(internal_tool_error(
                NAME,
                format!("failed to run verify command `{}`: {e}", self.command),
            )),
            Ok(Ok(output)) => {
                let so = String::from_utf8_lossy(&output.stdout);
                let se = String::from_utf8_lossy(&output.stderr);
                let text = match (so.trim().is_empty(), se.trim().is_empty()) {
                    (false, false) => format!("{}\n{}", so.trim(), se.trim()),
                    (true, false) => se.trim().to_string(),
                    (false, true) => so.trim().to_string(),
                    (true, true) => String::new(),
                };
                let body = reduce_output(text, MAX_BYTES);
                let code_line = match output.status.code() {
                    Some(code) => format!("exit code: {code}"),
                    None => "exit code: terminated by signal".to_string(),
                };
                let content = if body.is_empty() {
                    code_line
                } else {
                    format!("{body}\n{code_line}")
                };
                Ok(ToolOutcome {
                    content,
                    is_error: !output.status.success(),
                    usage: None,
                    meta: None,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn run(command: &str, dir: &std::path::Path) -> ToolOutcome {
        VerifyTool::new(command)
            .execute(
                serde_json::json!({}),
                &ToolCtx {
                    engine: None,
                    working_dir: dir.to_path_buf(),
                    sandbox: Default::default(),
                },
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn passing_command_reports_ok_with_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = run("echo all-good", dir.path()).await;
        assert!(!outcome.is_error);
        assert!(outcome.content.contains("all-good"));
        assert!(outcome.content.contains("exit code: 0"));
    }

    #[tokio::test]
    async fn failing_command_is_an_error_outcome_with_output() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = run("echo 'test 1 failed' >&2; exit 3", dir.path()).await;
        assert!(outcome.is_error);
        assert!(outcome.content.contains("test 1 failed"));
        assert!(outcome.content.contains("exit code: 3"));
    }

    #[tokio::test]
    async fn runs_in_the_working_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker.txt"), "here").unwrap();
        let outcome = run("cat marker.txt", dir.path()).await;
        assert!(!outcome.is_error);
        assert!(outcome.content.contains("here"));
    }

    #[tokio::test]
    async fn oversized_output_is_reduced() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = run("seq 1 100000", dir.path()).await;
        assert!(
            outcome.content.len() < 20_000,
            "capped: {}",
            outcome.content.len()
        );
        assert!(outcome.content.contains("elided"));
    }

    #[tokio::test]
    async fn unknown_arguments_are_rejected_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let tool = VerifyTool::new("true");
        let outcome = tool
            .execute(
                serde_json::json!({"path": "x"}),
                &ToolCtx {
                    engine: None,
                    working_dir: dir.path().to_path_buf(),
                    sandbox: Default::default(),
                },
            )
            .await
            .unwrap();
        assert!(outcome.is_error);
        assert!(outcome.content.contains("invalid arguments"));
    }
}
