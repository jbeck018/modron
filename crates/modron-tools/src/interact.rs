//! `interact` tool — drive an interactive TUI (vim, ssh, REPLs, dialogs)
//! through a persistent tmux session inside the environment.
//!
//! bash cannot run TUI programs: no tty, and pipes close when the call
//! ends (terminal-bench vim-terminal-task exposed exactly this). `interact`
//! keeps a tmux session alive across calls: `start` launches a command in
//! it, `send` types raw keys (with an optional trailing Enter), `capture`
//! returns the visible screen. The session outlives every tool call, so
//! stateful screen programs keep running while the model inspects them.

use crate::common::{internal_tool_error, tool_err};
use crate::reduce::reduce_output;
use crate::validate::validate_args;
use async_trait::async_trait;
use modron_core::hooks::scrub_tokio_command;
use modron_core::{Result, Tool, ToolCtx, ToolOutcome, ToolSpec};
use serde_json::{json, Value};
use std::process::Stdio;

const NAME: &str = "interact";
const MAX_BYTES: usize = 8 * 1024;
const DEFAULT_SESSION: &str = "modron-interact";

pub struct InteractTool;

impl InteractTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for InteractTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for InteractTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: NAME.into(),
            description:
                "Drive an interactive TUI program (vim, ssh, REPLs, pagers) in a persistent \
                 tmux session. bash cannot run screen programs; this can. Actions: \
                 `start` launches `command` in a new session; `send` types `keys` \
                 (literal text; set enter:true to press Enter after); `capture` returns \
                 the visible screen text. The session persists across calls."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["start", "send", "capture"],
                               "description": "start | send | capture."},
                    "command": {"type": "string",
                                "description": "(start) the command to launch."},
                    "keys": {"type": "string",
                             "description": "(send) keys to type, e.g. \"gg\" or \":wq\"."},
                    "enter": {"type": "boolean",
                              "description": "(send) press Enter after the keys."},
                    "session": {"type": "string",
                                "description": "Session name (default modron-interact)."}
                },
                "required": ["action"]
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        let spec = self.spec();
        if let Err(message) = validate_args(&spec, &args) {
            return Ok(tool_err(
                NAME,
                format!("invalid arguments: {message}"),
                MAX_BYTES,
            ));
        }
        let session = args
            .get("session")
            .and_then(|v| v.as_str())
            .unwrap_or(DEFAULT_SESSION);
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let run = |cmd: Vec<&str>| {
            // D9: scrub the tmux client too. The user command runs under the
            // tmux server, which takes its environment from the client that
            // started it; a server that is already running keeps its own.
            let mut tmux = tokio::process::Command::new("tmux");
            scrub_tokio_command(&mut tmux);
            tmux.args(&cmd)
                .current_dir(&ctx.working_dir)
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output()
        };
        let outcome = match action {
            "start" => {
                let Some(command) = args.get("command").and_then(|v| v.as_str()) else {
                    return Ok(tool_err(NAME, "start requires `command`", MAX_BYTES));
                };
                let out = run(vec!["new-session", "-d", "-s", session, "-c", ".", command])
                    .await
                    .map_err(|e| internal_tool_error(NAME, format!("tmux spawn failed: {e}")))?;
                if out.status.success() {
                    // Small settle delay, then show the first screen so the
                    // model sees the program actually came up.
                    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
                    let screen = run(vec!["capture-pane", "-t", session, "-p"])
                        .await
                        .map_err(|e| internal_tool_error(NAME, e.to_string()))?;
                    ToolOutcome::ok(reduce_output(
                        format!(
                            "started `{command}` in session `{session}`\n{}",
                            String::from_utf8_lossy(&screen.stdout)
                        ),
                        MAX_BYTES,
                    ))
                } else {
                    ToolOutcome::err(format!(
                        "tmux start failed: {}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    ))
                }
            }
            "send" => {
                let Some(keys) = args.get("keys").and_then(|v| v.as_str()) else {
                    return Ok(tool_err(NAME, "send requires `keys`", MAX_BYTES));
                };
                let enter = args.get("enter").and_then(|v| v.as_bool()).unwrap_or(false);
                let mut cmd = vec!["send-keys", "-t", session, "-l", keys];
                if enter {
                    cmd.push("Enter");
                }
                let out = run(cmd)
                    .await
                    .map_err(|e| internal_tool_error(NAME, format!("tmux spawn failed: {e}")))?;
                if !out.status.success() {
                    return Ok(ToolOutcome::err(format!(
                        "tmux send failed (session `{session}` alive?): {}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    )));
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                let screen = run(vec!["capture-pane", "-t", session, "-p"])
                    .await
                    .map_err(|e| internal_tool_error(NAME, e.to_string()))?;
                ToolOutcome::ok(reduce_output(
                    String::from_utf8_lossy(&screen.stdout).into_owned(),
                    MAX_BYTES,
                ))
            }
            "capture" => {
                let screen = run(vec!["capture-pane", "-t", session, "-p"])
                    .await
                    .map_err(|e| internal_tool_error(NAME, e.to_string()))?;
                ToolOutcome::ok(reduce_output(
                    String::from_utf8_lossy(&screen.stdout).into_owned(),
                    MAX_BYTES,
                ))
            }
            other => {
                return Ok(tool_err(
                    NAME,
                    format!("unknown action `{other}` (start|send|capture)"),
                    MAX_BYTES,
                ))
            }
        };
        Ok(outcome)
    }
}
