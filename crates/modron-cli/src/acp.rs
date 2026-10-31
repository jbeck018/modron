//! modron-acp — Agent Client Protocol server (Phase 5d X1): Modron inside
//! editors that speak ACP (Zed first). JSON-RPC 2.0 over stdio, mirroring
//! the MCP client's transport discipline.
//!
//! Implemented surface (the subset an editor needs for a working agent):
//! - `initialize` → protocol version + capabilities
//! - `session/new` → creates an agent session bound to a working directory
//! - `session/prompt` → runs the engine (first prompt: `run_task`;
//!   subsequent: `run_task_resuming` — ACP sessions map to Modron sessions)
//! - `session/update` notifications: `agent_message_chunk` (streamed text),
//!   `tool_call` / `tool_call_update` (live tool lines)
//! - prompt result: `{stopReason: "end_turn" | "max_tokens" | "refusal" | …}`
//!
//! Deliberately not yet: editor-side permission requests (`session/
//! request_permission` — approvals run with the engine's non-interactive
//! policy this tranche), fs proxies (Modron tools run locally in the session
//! cwd), MCP-over-ACP passthrough. Both are additive surface, not redesign.

use crate::{build_agent_with, config, spawn_mcp_servers, AgentSinks, Cli};
use modron_core::{StreamDelta, ToolActivity};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{mpsc, Arc, Mutex};

/// Events from the engine/UI layer that become ACP `session/update`
/// notifications.
pub enum AcUpdate {
    Delta(StreamDelta),
    Tool(ToolActivity),
}

/// One JSON-RPC message read from stdin (line-delimited).
pub fn parse_message(line: &str) -> Option<Value> {
    if line.trim().is_empty() {
        return None;
    }
    serde_json::from_str(line.trim()).ok()
}

/// The `initialize` result. Version 1 is the current ACP protocol version.
pub fn initialize_result() -> Value {
    json!({
        "protocolVersion": 1,
        "agentCapabilities": {
            "loadSession": false,
            "promptCapabilities": {"embeddedContext": false}
        },
        "authMethods": []
    })
}

/// Build a `session/update` notification for a streamed text chunk.
pub fn message_chunk_notification(session_id: &str, text: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": session_id,
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": text}
            }
        }
    })
}

/// Build a `session/update` for a tool call (start or completion).
pub fn tool_call_notification(
    session_id: &str,
    tool_call_id: &str,
    title: &str,
    status: &str,
) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": session_id,
            "update": {
                "sessionUpdate": "tool_call",
                "toolCallId": tool_call_id,
                "title": title,
                "kind": "read",
                "status": status
            }
        }
    })
}

/// Map an engine stop reason to the ACP stopReason vocabulary.
pub fn stop_reason_to_acp(reason: modron_core::StopReason) -> &'static str {
    match reason {
        modron_core::StopReason::EndTurn => "end_turn",
        modron_core::StopReason::MaxTokens => "max_tokens",
        modron_core::StopReason::ToolUse | modron_core::StopReason::Other => "end_turn",
    }
}

/// Pure request router (unit-testable): given a request method and params,
/// decide the response result and whether a session was created. The run
/// loop owns I/O; this owns semantics.
pub fn route_initialize(params: &Value) -> Value {
    let _ = params;
    initialize_result()
}

/// Validate a session/new request: returns the working directory.
pub fn session_new_cwd(params: &Value) -> Option<std::path::PathBuf> {
    params
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(std::path::PathBuf::from)
}

/// Extract the text of a session/prompt request (concatenated text blocks).
pub fn prompt_text(params: &Value) -> Option<String> {
    let blocks = params.get("prompt")?.as_array()?;
    let mut text = String::new();
    for block in blocks {
        if block.get("type").and_then(|v| v.as_str()) == Some("text") {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(block.get("text").and_then(|v| v.as_str()).unwrap_or(""));
        }
    }
    Some(text).filter(|t| !t.trim().is_empty())
}

/// Frame an update channel: engine deltas/tools → serialized JSON lines for
/// stdout. The run loop owns the writer; this converts.
pub fn update_to_line(
    update: &AcUpdate,
    session_id: &str,
    tool_call_counter: &mut u64,
) -> Option<String> {
    match update {
        AcUpdate::Delta(StreamDelta::Text(t)) if !t.is_empty() => {
            Some(message_chunk_notification(session_id, t).to_string())
        }
        AcUpdate::Delta(_) => None,
        AcUpdate::Tool(ToolActivity::Start { name }) => {
            *tool_call_counter += 1;
            Some(
                tool_call_notification(
                    session_id,
                    &format!("call-{tool_call_counter}"),
                    name,
                    "in_progress",
                )
                .to_string(),
            )
        }
        AcUpdate::Tool(ToolActivity::Done { name, is_error, .. }) => {
            let status = if *is_error { "failed" } else { "completed" };
            Some(tool_call_notification(session_id, name, name, status).to_string())
        }
    }
}

/// Run the ACP server over stdio (Phase 5d X1). The agent is built from the
/// same CLI config as every other front end. ACP sessions map onto Modron
/// sessions lazily: `session/new` returns an opaque id; the first
/// `session/prompt` creates the Modron session, later prompts resume it.
pub(super) async fn run_acp(cli: &Cli, cfg: &config::Config) -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let mcp = spawn_mcp_servers(cfg).await?;
    let (update_tx, update_rx) = mpsc::channel::<AcUpdate>();
    let delta_tx = update_tx.clone();
    let tool_tx = update_tx.clone();
    let agent = Arc::new(build_agent_with(
        cli,
        cfg,
        false, // editor-side approvals land with request_permission (next)
        AgentSinks {
            approver: None,
            on_delta: Some(Arc::new(move |d: StreamDelta| {
                let _ = delta_tx.send(AcUpdate::Delta(d));
            })),
            on_tool_event: Some(Arc::new(move |t: ToolActivity| {
                let _ = tool_tx.send(AcUpdate::Tool(t));
            })),
        },
        &mcp,
    )?);

    // ACP session id → Modron session id (None until the first prompt ran).
    let sessions: Arc<Mutex<HashMap<String, Option<modron_core::SessionId>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel::<(
        String,
        Value,
        modron_core::Result<modron_core::RunSummary>,
    )>();

    let stdout = Arc::new(tokio::sync::Mutex::new(tokio::io::BufWriter::new(
        tokio::io::stdout(),
    )));
    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let mut stdin_lines = stdin.lines();
    let mut engine_task: Option<tokio::task::JoinHandle<()>> = None;
    // The in-flight prompt: (acp session id, JSON-RPC request id).
    let mut pending: Option<(String, Value)> = None;
    let mut tool_call_counter: u64 = 0;
    let result: anyhow::Result<()> = Ok(());

    loop {
        tokio::select! {
            line = stdin_lines.next_line() => {
                let Some(line) = line? else { break };
                let Some(message) = parse_message(&line) else { continue };
                let Some(method) = message.get("method").and_then(|m| m.as_str()).map(str::to_owned) else {
                    continue; // a response to a client-side request we did not send
                };
                let id = message.get("id").cloned();
                match method.as_str() {
                    "initialize" => {
                        write_response(&stdout, id, route_initialize(&message["params"])).await?;
                    }
                    "session/new" => {
                        let _ = session_new_cwd(&message["params"]);
                        let acp_session = uuid::Uuid::new_v4().to_string();
                        sessions.lock().unwrap().insert(acp_session.clone(), None);
                        write_response(&stdout, id, json!({"sessionId": acp_session})).await?;
                    }
                    "session/prompt" => {
                        let Some(session_id) = message.pointer("/params/sessionId").and_then(|v| v.as_str()).map(str::to_owned) else {
                            write_error(&stdout, id, -32602, "missing sessionId").await?;
                            continue;
                        };
                        let Some(task) = prompt_text(&message["params"]) else {
                            write_error(&stdout, id, -32602, "empty prompt").await?;
                            continue;
                        };
                        if pending.is_some() {
                            write_error(&stdout, id, -32000, "a prompt is already running").await?;
                            continue;
                        }
                        pending = Some((session_id.clone(), id.clone().unwrap_or(json!(null))));
                        let agent = Arc::clone(&agent);
                        let sessions = Arc::clone(&sessions);
                        let done = done_tx.clone();
                        let sid = session_id.clone();
                        engine_task = Some(tokio::spawn(async move {
                            let modron_session =
                                sessions.lock().unwrap().get(&sid).cloned().flatten();
                            let outcome = match modron_session {
                                Some(id) => agent.engine.run_task_resuming(id, &task).await,
                                None => agent.engine.run_task(&task).await,
                            };
                            let _ = done.send((sid, id.unwrap_or(json!(null)), outcome));
                        }));
                    }
                    _ => {
                        if id.is_some() {
                            write_error(&stdout, id, -32601, "method not found").await?;
                        }
                    }
                }
            }
            Some((session_id, request_id, outcome)) = done_rx.recv() => {
                engine_task = None;
                if let Some(map) = sessions.lock().unwrap().get_mut(&session_id) {
                    if map.is_none() {
                        *map = outcome.as_ref().ok().map(|s| s.session_id);
                    }
                }
                // Flush every update BEFORE the response: the editor must see
                // the streamed chunks and tool calls before end_turn.
                while let Ok(update) = update_rx.try_recv() {
                    if let Some(line) = update_to_line(&update, &session_id, &mut tool_call_counter) {
                        let mut out = stdout.lock().await;
                        out.write_all(line.as_bytes()).await?;
                        out.write_all(b"\n").await?;
                        out.flush().await?;
                    }
                }
                match outcome {
                    Ok(summary) => {
                        // Per-prompt usage lands in the transcript (the
                        // engine already persisted it); editors see stopReason.
                        write_response(&stdout, Some(request_id), json!({"stopReason": stop_reason_to_acp(modron_core::StopReason::EndTurn)})).await?;
                        let _ = summary;
                    }
                    Err(e) => {
                        write_error(&stdout, Some(request_id), -32000, &e.to_string()).await?;
                    }
                }
                pending = None;
            }
        }

        // Drain engine updates → session/update notifications.
        while let Ok(update) = update_rx.try_recv() {
            if let Some((session_id, _)) = pending.as_ref() {
                if let Some(line) = update_to_line(&update, session_id, &mut tool_call_counter) {
                    let mut out = stdout.lock().await;
                    out.write_all(line.as_bytes()).await?;
                    out.write_all(b"\n").await?;
                    out.flush().await?;
                }
            }
        }
        // Pending prompt whose task already finished in the drain window.
        if let Some((session_id, request_id)) = pending.clone() {
            let finished = engine_task
                .as_ref()
                .map(|t| t.is_finished())
                .unwrap_or(true);
            if finished && done_rx.try_recv().is_ok() {
                if let Some(task) = engine_task.take() {
                    let _ = task.await;
                }
                write_response(&stdout, Some(request_id), json!({"stopReason": "end_turn"}))
                    .await?;
                pending = None;
            }
            let _ = session_id;
        }
    }

    result
}

async fn write_response(
    stdout: &Arc<tokio::sync::Mutex<tokio::io::BufWriter<tokio::io::Stdout>>>,
    id: Option<Value>,
    result: Value,
) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;
    let message = json!({"jsonrpc": "2.0", "id": id.unwrap_or(json!(null)), "result": result});
    let mut out = stdout.lock().await;
    out.write_all(message.to_string().as_bytes()).await?;
    out.write_all(b"\n").await?;
    out.flush().await?;
    Ok(())
}

async fn write_error(
    stdout: &Arc<tokio::sync::Mutex<tokio::io::BufWriter<tokio::io::Stdout>>>,
    id: Option<Value>,
    code: i64,
    message: &str,
) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;
    let message = json!({
        "jsonrpc": "2.0", "id": id.unwrap_or(json!(null)),
        "error": {"code": code, "message": message}
    });
    let mut out = stdout.lock().await;
    out.write_all(message.to_string().as_bytes()).await?;
    out.write_all(b"\n").await?;
    out.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_reports_v1_capabilities() {
        let result = route_initialize(&json!({}));
        assert_eq!(result["protocolVersion"], 1);
        assert_eq!(result["agentCapabilities"]["loadSession"], false);
    }

    #[test]
    fn session_new_extracts_cwd() {
        let cwd = session_new_cwd(&json!({"cwd": "/tmp/project", "mcpServers": []}));
        assert_eq!(cwd, Some(std::path::PathBuf::from("/tmp/project")));
        assert_eq!(session_new_cwd(&json!({})), None);
    }

    #[test]
    fn prompt_text_joins_text_blocks_only() {
        let text = prompt_text(&json!({"prompt": [
            {"type": "text", "text": "fix the"},
            {"type": "image", "data": "..."},
            {"type": "text", "text": "bug"}
        ]}))
        .unwrap();
        assert_eq!(text, "fix the\nbug");
        assert!(prompt_text(&json!({"prompt": []})).is_none());
    }

    #[test]
    fn updates_map_to_notifications() {
        let mut counter = 0;
        let chunk = update_to_line(
            &AcUpdate::Delta(StreamDelta::Text("hello".into())),
            "s1",
            &mut counter,
        )
        .unwrap();
        assert!(chunk.contains("agent_message_chunk"));
        assert!(chunk.contains("hello"));

        let tool = update_to_line(
            &AcUpdate::Tool(ToolActivity::Start {
                name: "bash".into(),
            }),
            "s1",
            &mut counter,
        )
        .unwrap();
        assert!(tool.contains("tool_call"));
        assert!(tool.contains("in_progress"));
    }
}
