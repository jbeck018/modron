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
use std::sync::atomic::Ordering;
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
            "loadSession": true,
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

/// Build a `session/request_permission` agent→client request for one tool
/// call (Phase: ACP permission requests). Options follow the ACP vocabulary;
/// the client answers with `{outcome: {outcome: "selected", optionId}}` or
/// `{outcome: {outcome: "cancelled"}}`.
pub fn permission_request(
    session_id: &str,
    tool_call_id: &str,
    title: &str,
    request_id: u64,
) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "session/request_permission",
        "params": {
            "sessionId": session_id,
            "toolCall": {"toolCallId": tool_call_id, "title": title},
            "options": [
                {"kind": "allow_once", "id": "allow", "name": "Allow"},
                {"kind": "reject_once", "id": "deny", "name": "Deny"}
            ]
        }
    })
}

/// Parse the client's outcome: true = allow.
pub fn parse_permission_response(response: &Value) -> bool {
    let outcome = response
        .get("outcome")
        .or_else(|| response.pointer("/result/outcome"));
    match outcome {
        Some(o) if o.get("outcome").and_then(|v| v.as_str()) == Some("selected") => {
            o.get("optionId").and_then(|v| v.as_str()) == Some("allow")
        }
        _ => false,
    }
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

/// `session/load`: the editor passes a Modron session id (from a previous
/// run's footer or `modron sessions`); we hand back a fresh ACP session id
/// whose first prompt will resume that Modron session.
pub fn load_session_result(session_id: &str) -> Value {
    json!({"sessionId": session_id})
}

/// Validate a session/load request: returns the Modron session id to bind.
pub fn session_load_id(params: &Value) -> Option<String> {
    params
        .get("sessionId")
        .and_then(|v| v.as_str())
        .map(String::from)
        .filter(|s| !s.is_empty())
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
        AcUpdate::Tool(ToolActivity::SubagentStarted { id, label }) => Some(
            tool_call_notification(session_id, id, &format!("subagent: {label}"), "in_progress")
                .to_string(),
        ),
        AcUpdate::Tool(ToolActivity::SubagentProgress {
            id,
            turns,
            tokens,
            last_tool,
            ..
        }) => {
            let note = match last_tool {
                Some(tool) => format!("turn {turns} · {tokens} tok · {tool}"),
                None => format!("turn {turns} · {tokens} tok"),
            };
            Some(tool_call_notification(session_id, id, &note, "in_progress").to_string())
        }
        AcUpdate::Tool(ToolActivity::SubagentFinished {
            id,
            tokens,
            turns,
            is_error,
            ..
        }) => {
            let status = if *is_error { "failed" } else { "completed" };
            Some(
                tool_call_notification(
                    session_id,
                    id,
                    &format!("{turns} turns · {tokens} tok"),
                    status,
                )
                .to_string(),
            )
        }
    }
}

/// Run the ACP server over stdio (Phase 5d X1). The agent is built from the
/// same CLI config as every other front end. ACP sessions map onto Modron
/// sessions lazily: `session/new` returns an opaque id; the first
/// `session/prompt` creates the Modron session, later prompts resume it.
///
/// Approvals: mutating tools in Ask mode raise `session/request_permission`
/// against the editor (allow/deny once) — the safety gate survives the
/// interface change, per PLAN §3.5.
pub(crate) async fn run_acp(cli: &Cli, cfg: &config::Config) -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let mcp = spawn_mcp_servers(cfg).await?;

    // Single-writer stdout: every emission (responses, errors, updates,
    // permission requests) goes through this channel in order.
    let (write_tx, mut write_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::BufWriter::new(tokio::io::stdout());
        while let Some(line) = write_rx.recv().await {
            if stdout.write_all(line.as_bytes()).await.is_err()
                || stdout.write_all(b"\n").await.is_err()
                || stdout.flush().await.is_err()
            {
                break;
            }
        }
    });

    // Pending permission requests: id → decision channel.
    let pending_permissions: Arc<Mutex<HashMap<u64, mpsc::Sender<bool>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let permission_ids = Arc::new(std::sync::atomic::AtomicU64::new(1));
    let current_acp_session: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let write_for_approver = write_tx.clone();
    let approver_map = Arc::clone(&pending_permissions);
    let approver_ids = Arc::clone(&permission_ids);
    let approver_session = Arc::clone(&current_acp_session);

    let (update_tx, update_rx) = mpsc::channel::<AcUpdate>();
    let delta_tx = update_tx.clone();
    let tool_tx = update_tx.clone();
    let agent = Arc::new(build_agent_with(
        cli,
        cfg,
        true, // the editor is the approver
        AgentSinks {
            approver: Some(Arc::new(move |call: &modron_core::ToolCall| {
                let request_id = approver_ids.fetch_add(1, Ordering::Relaxed);
                let session = approver_session
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string());
                let (respond, receive) = mpsc::channel();
                approver_map.lock().unwrap().insert(request_id, respond);
                let request = permission_request(
                    &session,
                    &format!("call-{request_id}"),
                    &format!("{} {}", call.name, short_call_summary(call)),
                    request_id,
                );
                let _ = write_for_approver.send(request.to_string());
                // Park the engine task until the editor answers.
                tokio::task::block_in_place(|| receive.recv().unwrap_or(false))
            })),
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

    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let mut stdin_lines = stdin.lines();
    let mut pending: Option<(String, Value)> = None;
    let mut tool_call_counter: u64 = 0;
    let result: anyhow::Result<()> = Ok(());

    macro_rules! send {
        ($line:expr) => {
            let _ = write_tx.send($line);
        };
    }

    loop {
        tokio::select! {
            line = stdin_lines.next_line() => {
                let Some(line) = line? else { break };
                let Some(message) = parse_message(&line) else { continue };
                let id = message.get("id").cloned();
                // Responses to OUR requests (permission decisions) route by id.
                if message.get("method").is_none() {
                    if let Some(respond) = id
                        .as_ref()
                        .and_then(|v| v.as_u64())
                        .and_then(|pid| pending_permissions.lock().unwrap().remove(&pid))
                    {
                        let allow = parse_permission_response(&message);
                        let _ = respond.send(allow);
                    }
                    continue;
                }
                let Some(method) = message.get("method").and_then(|m| m.as_str()).map(str::to_owned) else {
                    continue;
                };
                match method.as_str() {
                    "initialize" => {
                        send!(json_response(id, route_initialize(&message["params"])));
                    }
                    "session/load" => {
                        let Some(modron_id) = session_load_id(&message["params"]) else {
                            send!(json_error(id, -32602, "missing sessionId"));
                            continue;
                        };
                        // Validate it parses as a Modron session id.
                        match modron_id.parse::<modron_core::SessionId>() {
                            Ok(parsed) => {
                                let acp_session = uuid::Uuid::new_v4().to_string();
                                *current_acp_session.lock().unwrap() = Some(acp_session.clone());
                                sessions
                                    .lock()
                                    .unwrap()
                                    .insert(acp_session.clone(), Some(parsed));
                                send!(json_response(id, json!({"sessionId": acp_session})));
                            }
                            Err(_) => {
                                send!(json_error(id, -32602, "invalid sessionId (expected a Modron session uuid)"));
                            }
                        }
                    }
                    "session/new" => {
                        let _ = session_new_cwd(&message["params"]);
                        let acp_session = uuid::Uuid::new_v4().to_string();
                        *current_acp_session.lock().unwrap() = Some(acp_session.clone());
                        sessions.lock().unwrap().insert(acp_session.clone(), None);
                        send!(json_response(id, json!({"sessionId": acp_session})));
                    }
                    "session/prompt" => {
                        let Some(session_id) = message.pointer("/params/sessionId").and_then(|v| v.as_str()).map(str::to_owned) else {
                            send!(json_error(id, -32602, "missing sessionId"));
                            continue;
                        };
                        let Some(task) = prompt_text(&message["params"]) else {
                            send!(json_error(id, -32602, "empty prompt"));
                            continue;
                        };
                        if pending.is_some() {
                            send!(json_error(id, -32000, "a prompt is already running"));
                            continue;
                        }
                        pending = Some((session_id.clone(), id.clone().unwrap_or(json!(null))));
                        let agent = Arc::clone(&agent);
                        let sessions = Arc::clone(&sessions);
                        let done = done_tx.clone();
                        let sid = session_id.clone();
                        tokio::spawn(async move {
                            let modron_session =
                                sessions.lock().unwrap().get(&sid).cloned().flatten();
                            let outcome = match modron_session {
                                Some(id) => agent.engine.run_task_resuming(id, &task).await,
                                None => agent.engine.run_task(&task).await,
                            };
                            let _ = done.send((sid, id.unwrap_or(json!(null)), outcome));
                        });
                    }
                    _ => {
                        if id.is_some() {
                            send!(json_error(id, -32601, "method not found"));
                        }
                    }
                }
            }
            Some((session_id, request_id, outcome)) = done_rx.recv() => {
                if let Some(map) = sessions.lock().unwrap().get_mut(&session_id) {
                    if map.is_none() {
                        *map = outcome.as_ref().ok().map(|s| s.session_id);
                    }
                }
                // Flush every update BEFORE the response: the editor must see
                // the streamed chunks and tool calls before end_turn.
                while let Ok(update) = update_rx.try_recv() {
                    if let Some(line) = update_to_line(&update, &session_id, &mut tool_call_counter) {
                        send!(line);
                    }
                }
                match outcome {
                    Ok(summary) => {
                        send!(json_response(Some(request_id), json!({"stopReason": stop_reason_to_acp(modron_core::StopReason::EndTurn)})));
                        let _ = summary;
                    }
                    Err(e) => {
                        send!(json_error(Some(request_id), -32000, &e.to_string()));
                    }
                }
                pending = None;
            }
        }

        // Drain engine updates → session/update notifications.
        while let Ok(update) = update_rx.try_recv() {
            if let Some((session_id, _)) = pending.as_ref() {
                if let Some(line) = update_to_line(&update, session_id, &mut tool_call_counter) {
                    send!(line);
                }
            }
        }
    }

    drop(write_tx);
    let _ = writer.await;
    result
}

fn short_call_summary(call: &modron_core::ToolCall) -> String {
    let args = call.arguments.to_string();
    if args.chars().count() > 80 {
        format!("{}…", args.chars().take(80).collect::<String>())
    } else {
        args
    }
}

fn json_response(id: Option<Value>, result: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id.unwrap_or(json!(null)), "result": result}).to_string()
}

fn json_error(id: Option<Value>, code: i64, message: &str) -> String {
    json!({
        "jsonrpc": "2.0", "id": id.unwrap_or(json!(null)),
        "error": {"code": code, "message": message}
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_reports_v1_capabilities() {
        let result = route_initialize(&json!({}));
        assert_eq!(result["protocolVersion"], 1);
        assert_eq!(result["agentCapabilities"]["loadSession"], true);
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
    fn load_session_parses_and_advertises() {
        let result = route_initialize(&json!({}));
        assert_eq!(result["agentCapabilities"]["loadSession"], true);
        assert_eq!(load_session_result("abc"), json!({"sessionId": "abc"}));
        assert_eq!(
            session_load_id(&json!({"sessionId": "abc"})),
            Some("abc".into())
        );
        assert_eq!(session_load_id(&json!({})), None);
    }

    #[test]
    fn permission_requests_and_outcomes() {
        let request = permission_request("s1", "call-1", "bash echo hi", 42);
        assert_eq!(request["id"], 42);
        assert_eq!(request["method"], "session/request_permission");
        assert_eq!(request["params"]["options"][0]["kind"], "allow_once");

        assert!(parse_permission_response(&json!({
            "outcome": {"outcome": "selected", "optionId": "allow"}
        })));
        assert!(!parse_permission_response(&json!({
            "outcome": {"outcome": "selected", "optionId": "deny"}
        })));
        assert!(!parse_permission_response(&json!({
            "outcome": {"outcome": "cancelled"}
        })));
        assert!(!parse_permission_response(&json!({})));
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
