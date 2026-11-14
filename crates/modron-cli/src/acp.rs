//! modron-acp — Agent Client Protocol server (Phase 5d X1): Modron inside
//! editors that speak ACP (Zed first). JSON-RPC 2.0 over stdio, mirroring
//! the MCP client's transport discipline.
//!
//! Implemented surface (the subset an editor needs for a working agent):
//! - `initialize` → protocol version + capabilities
//! - `session/new` → creates an agent session bound to a working directory
//! - `session/load` → binds an ACP session to an existing Modron session
//! - `session/prompt` → runs the engine (first prompt: `run_task`;
//!   subsequent: `run_task_resuming` — ACP sessions map to Modron sessions).
//!   An unknown `sessionId` is a JSON-RPC error (-32602).
//! - `session/cancel` (notification) → aborts the running prompt; the
//!   prompt answers `{stopReason: "cancelled"}`
//! - `session/update` notifications: `agent_message_chunk` (streamed text),
//!   `tool_call` (start) and `tool_call_update` (completion, same
//!   `call-<n>` id as the start)
//! - prompt result: `{stopReason: "end_turn" | "max_tokens" | "refusal" | …}`
//!
//! Ordering: every outbound line (updates and permission requests alike)
//! goes through one queue and is written as it arrives, in the order the
//! engine produced it.
//!
//! Deliberately not yet: fs proxies (Modron tools run locally in the session
//! cwd), MCP-over-ACP passthrough.

use crate::{build_agent_with, config, spawn_mcp_servers, AgentSinks, Cli};
use modron_core::{StreamDelta, ToolActivity};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Mutex, MutexGuard};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

/// Events from the engine/UI layer that become ACP `session/update`
/// notifications.
pub enum AcUpdate {
    Delta(StreamDelta),
    Tool(ToolActivity),
}

/// One item the engine side hands to the run loop, tagged with the prompt
/// run that produced it. The loop writes each one as it arrives.
pub enum RunEvent {
    /// A streamed delta or tool activity.
    Update { run: u64, update: AcUpdate },
    /// A complete outbound line (a permission request).
    Line { run: u64, line: String },
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

/// Build a `session/update` that starts a tool call.
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

/// Build a `session/update` that changes an existing tool call (its status,
/// and optionally its title). `tool_call_id` is the id the start used.
pub fn tool_call_update_notification(
    session_id: &str,
    tool_call_id: &str,
    title: Option<&str>,
    status: &str,
) -> Value {
    let mut update = json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": tool_call_id,
        "status": status
    });
    if let Some(title) = title {
        update["title"] = json!(title);
    }
    json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {"sessionId": session_id, "update": update}
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

/// The effective session cwd: the editor-supplied value when present and a
/// real directory, else the launch directory. (Tested pure.)
pub fn session_cwd(params: &Value, launch_dir: &std::path::Path) -> std::path::PathBuf {
    match session_new_cwd(params) {
        Some(cwd) if cwd.is_dir() => cwd,
        _ => launch_dir.to_path_buf(),
    }
}

/// Build the per-session Engine: a cheap clone of the base engine with
/// `working_dir` pinned to the session cwd — every tool call (bash, edit,
/// write, skills) then runs in the editor's project, not the launch dir.
/// Providers, tool registry, store, and background-subagent registry are
/// Arc-shared across sessions.
fn session_engine(
    base: &modron_core::Engine,
    params: &Value,
    launch_dir: &std::path::Path,
) -> modron_core::Engine {
    let mut engine = base.clone();
    engine.config.working_dir = session_cwd(params, launch_dir);
    engine
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

/// Tool-call id bookkeeping for one prompt run. Engine tool activity carries
/// only the tool name, so a completion is matched to its start by name, first
/// in first out. Ids are `call-<n>`; a completion with no open start gets a
/// fresh id rather than reusing another call's.
#[derive(Default)]
pub struct ToolCallIds {
    counter: u64,
    open: HashMap<String, VecDeque<String>>,
}

impl ToolCallIds {
    fn start(&mut self, name: &str) -> String {
        self.counter += 1;
        let id = format!("call-{}", self.counter);
        self.open
            .entry(name.to_string())
            .or_default()
            .push_back(id.clone());
        id
    }

    fn finish(&mut self, name: &str) -> String {
        match self.open.get_mut(name).and_then(VecDeque::pop_front) {
            Some(id) => id,
            None => {
                self.counter += 1;
                format!("call-{}", self.counter)
            }
        }
    }
}

fn status_for(is_error: bool) -> &'static str {
    if is_error {
        "failed"
    } else {
        "completed"
    }
}

/// Frame one engine update as a `session/update` line for `session_id`.
/// Tool completions reuse the start's `call-<n>` id from `tools`.
pub fn update_to_line(
    update: &AcUpdate,
    session_id: &str,
    tools: &mut ToolCallIds,
) -> Option<String> {
    match update {
        AcUpdate::Delta(StreamDelta::Text(t)) if !t.is_empty() => {
            Some(message_chunk_notification(session_id, t).to_string())
        }
        AcUpdate::Delta(_) => None,
        AcUpdate::Tool(ToolActivity::Start { name }) => {
            let id = tools.start(name);
            Some(tool_call_notification(session_id, &id, name, "in_progress").to_string())
        }
        AcUpdate::Tool(ToolActivity::Done { name, is_error, .. }) => {
            let id = tools.finish(name);
            Some(
                tool_call_update_notification(session_id, &id, None, status_for(*is_error))
                    .to_string(),
            )
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
            Some(
                tool_call_update_notification(session_id, id, Some(&note), "in_progress")
                    .to_string(),
            )
        }
        AcUpdate::Tool(ToolActivity::SubagentFinished {
            id,
            tokens,
            turns,
            is_error,
            ..
        }) => Some(
            tool_call_update_notification(
                session_id,
                id,
                Some(&format!("{turns} turns · {tokens} tok")),
                status_for(*is_error),
            )
            .to_string(),
        ),
    }
}

/// The prompt run currently executing, as the engine-side sinks see it.
#[derive(Clone)]
struct ActiveRun {
    id: u64,
    session_id: String,
}

/// Shared state between the run loop and the engine sinks: which prompt run
/// is active (and its ACP session), the ordered event queue, and the open
/// permission requests.
pub(crate) struct AcpHub {
    events: mpsc::UnboundedSender<RunEvent>,
    active: Mutex<Option<ActiveRun>>,
    run_ids: AtomicU64,
    permission_ids: AtomicU64,
    pending_permissions: Mutex<HashMap<u64, std_mpsc::Sender<bool>>>,
}

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl AcpHub {
    /// The hub plus the receiver the run loop drains, in order.
    pub(crate) fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<RunEvent>) {
        let (events, rx) = mpsc::unbounded_channel();
        let hub = Self {
            events,
            active: Mutex::new(None),
            run_ids: AtomicU64::new(1),
            permission_ids: AtomicU64::new(1),
            pending_permissions: Mutex::new(HashMap::new()),
        };
        (Arc::new(hub), rx)
    }

    /// Engine sinks wired to this hub (approvals, streamed deltas, tool
    /// activity). Events fired while no prompt is running are dropped.
    pub(crate) fn sinks(self: &Arc<Self>) -> AgentSinks {
        let approver_hub = Arc::clone(self);
        let delta_hub = Arc::clone(self);
        let tool_hub = Arc::clone(self);
        AgentSinks {
            approver: Some(Arc::new(move |call: &modron_core::ToolCall| {
                approver_hub.request_permission(call)
            })),
            on_delta: Some(Arc::new(move |d: StreamDelta| {
                delta_hub.emit(AcUpdate::Delta(d));
            })),
            on_tool_event: Some(Arc::new(move |t: ToolActivity| {
                tool_hub.emit(AcUpdate::Tool(t));
            })),
        }
    }

    fn next_run_id(&self) -> u64 {
        self.run_ids.fetch_add(1, Ordering::Relaxed)
    }

    fn begin_run(&self, run: u64, session_id: &str) {
        *locked(&self.active) = Some(ActiveRun {
            id: run,
            session_id: session_id.to_string(),
        });
    }

    fn end_run(&self) {
        *locked(&self.active) = None;
    }

    fn emit(&self, update: AcUpdate) {
        let active = locked(&self.active).clone();
        if let Some(run) = active {
            let _ = self.events.send(RunEvent::Update {
                run: run.id,
                update,
            });
        }
    }

    /// Engine approver: queue the permission request in the ordered stream,
    /// then park until the editor answers. The session is the run's own.
    fn request_permission(&self, call: &modron_core::ToolCall) -> bool {
        let active = locked(&self.active).clone();
        let Some(run) = active else {
            return false;
        };
        let request_id = self.permission_ids.fetch_add(1, Ordering::Relaxed);
        let (respond, receive) = std_mpsc::channel();
        locked(&self.pending_permissions).insert(request_id, respond);
        let line = permission_request(
            &run.session_id,
            &format!("call-{request_id}"),
            &format!("{} {}", call.name, short_call_summary(call)),
            request_id,
        )
        .to_string();
        let _ = self.events.send(RunEvent::Line { run: run.id, line });
        // Park the engine task until the editor answers (or the run is
        // cancelled, which denies it).
        tokio::task::block_in_place(|| receive.recv().unwrap_or(false))
    }

    fn answer_permission(&self, request_id: u64, allow: bool) {
        if let Some(respond) = locked(&self.pending_permissions).remove(&request_id) {
            let _ = respond.send(allow);
        }
    }

    fn deny_pending_permissions(&self) {
        for (_, respond) in locked(&self.pending_permissions).drain() {
            let _ = respond.send(false);
        }
    }
}

/// An ACP session: the Modron resume id plus a per-session Engine clone whose
/// working_dir follows the editor-supplied cwd.
struct AcSession {
    resume: Option<modron_core::SessionId>,
    engine: modron_core::Engine,
}

/// The prompt run the loop is waiting on.
struct Running {
    run: u64,
    session_id: String,
    request_id: Value,
    task: tokio::task::JoinHandle<()>,
    tools: ToolCallIds,
}

/// A finished run: (run id, ACP session id, engine outcome).
type Done = (u64, String, modron_core::Result<modron_core::RunSummary>);

/// The loop's state. Owns the single stdout queue and all session bookkeeping.
struct Server {
    write: mpsc::UnboundedSender<String>,
    base: modron_core::Engine,
    launch_dir: PathBuf,
    hub: Arc<AcpHub>,
    sessions: HashMap<String, AcSession>,
    running: Option<Running>,
    done: mpsc::UnboundedSender<Done>,
}

impl Server {
    fn reply(&self, line: String) {
        let _ = self.write.send(line);
    }

    fn reply_err(&self, id: Option<Value>, code: i64, message: &str) {
        self.reply(json_error(id, code, message));
    }

    fn new_session(&mut self, resume: Option<modron_core::SessionId>, params: &Value) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        let engine = session_engine(&self.base, params, &self.launch_dir);
        self.sessions
            .insert(id.clone(), AcSession { resume, engine });
        id
    }

    /// Route one inbound JSON-RPC message.
    fn on_message(&mut self, message: Value) {
        let id = message.get("id").cloned();
        let Some(method) = message
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            // A response to one of our requests: permission decisions route
            // by id.
            if let Some(request_id) = id.as_ref().and_then(Value::as_u64) {
                self.hub
                    .answer_permission(request_id, parse_permission_response(&message));
            }
            return;
        };
        let params = message["params"].clone();
        match method.as_str() {
            "initialize" => self.reply(json_response(id, route_initialize(&params))),
            "session/load" => match session_load_id(&params) {
                None => self.reply_err(id, -32602, "missing sessionId"),
                Some(modron_id) => match modron_id.parse::<modron_core::SessionId>() {
                    Ok(parsed) => {
                        let acp = self.new_session(Some(parsed), &params);
                        self.reply(json_response(id, load_session_result(&acp)));
                    }
                    Err(_) => self.reply_err(
                        id,
                        -32602,
                        "invalid sessionId (expected a Modron session uuid)",
                    ),
                },
            },
            "session/new" => {
                let acp = self.new_session(None, &params);
                self.reply(json_response(id, load_session_result(&acp)));
            }
            "session/prompt" => self.prompt(id, &params),
            "session/cancel" => {
                // A notification: no response is written for it.
                if let Some(session_id) = params.get("sessionId").and_then(Value::as_str) {
                    self.cancel(session_id);
                }
            }
            _ => {
                if id.is_some() {
                    self.reply_err(id, -32601, "method not found");
                }
            }
        }
    }

    fn prompt(&mut self, id: Option<Value>, params: &Value) {
        let Some(session_id) = params
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            return self.reply_err(id, -32602, "missing sessionId");
        };
        let Some(session) = self.sessions.get(&session_id) else {
            return self.reply_err(id, -32602, &format!("unknown sessionId: {session_id}"));
        };
        let resume = session.resume;
        let engine = session.engine.clone();
        let Some(task) = prompt_text(params) else {
            return self.reply_err(id, -32602, "empty prompt");
        };
        if self.running.is_some() {
            return self.reply_err(id, -32000, "a prompt is already running");
        }
        let run = self.hub.next_run_id();
        self.hub.begin_run(run, &session_id);
        let done = self.done.clone();
        let sid = session_id.clone();
        let handle = tokio::spawn(async move {
            let outcome = match resume {
                Some(modron_id) => engine.run_task_resuming(modron_id, &task).await,
                None => engine.run_task(&task).await,
            };
            let _ = done.send((run, sid, outcome));
        });
        self.running = Some(Running {
            run,
            session_id,
            request_id: id.unwrap_or(Value::Null),
            task: handle,
            tools: ToolCallIds::default(),
        });
    }

    /// Write one engine event for the running prompt. Events from any other
    /// run (a cancelled or finished one) are dropped.
    fn on_event(&mut self, event: RunEvent) {
        let Some(running) = self.running.as_mut() else {
            return;
        };
        match event {
            RunEvent::Update { run, update } if run == running.run => {
                if let Some(line) = update_to_line(&update, &running.session_id, &mut running.tools)
                {
                    let _ = self.write.send(line);
                }
            }
            RunEvent::Line { run, line } if run == running.run => {
                let _ = self.write.send(line);
            }
            _ => {}
        }
    }

    fn on_done(&mut self, done: Done, events: &mut mpsc::UnboundedReceiver<RunEvent>) {
        let (run, session_id, outcome) = done;
        // A cancelled run was already answered; its late completion is ignored.
        if self.running.as_ref().map(|r| r.run) != Some(run) {
            return;
        }
        // Everything the run emitted was queued before it finished: write it
        // all before the response so the editor sees text and tools first.
        while let Ok(event) = events.try_recv() {
            self.on_event(event);
        }
        let Some(running) = self.running.take() else {
            return;
        };
        self.hub.end_run();
        if let Ok(summary) = &outcome {
            if let Some(entry) = self.sessions.get_mut(&session_id) {
                if entry.resume.is_none() {
                    entry.resume = Some(summary.session_id);
                }
            }
        }
        match outcome {
            Ok(_) => self.reply(json_response(
                Some(running.request_id),
                json!({"stopReason": stop_reason_to_acp(modron_core::StopReason::EndTurn)}),
            )),
            Err(e) => self.reply(json_error(Some(running.request_id), -32000, &e.to_string())),
        }
    }

    /// Stop the running prompt, if any: deny its open permissions (so a parked
    /// approver returns), abort the task, and clear the active run.
    fn stop_running(&mut self) -> Option<Running> {
        let running = self.running.take()?;
        self.hub.deny_pending_permissions();
        running.task.abort();
        self.hub.end_run();
        Some(running)
    }

    fn cancel(&mut self, session_id: &str) {
        let is_running = self
            .running
            .as_ref()
            .is_some_and(|r| r.session_id == session_id);
        if !is_running {
            return;
        }
        if let Some(running) = self.stop_running() {
            self.reply(json_response(
                Some(running.request_id),
                json!({"stopReason": "cancelled"}),
            ));
        }
    }
}

/// Run the ACP server over `input`/`output` (Phase 5d X1). Production passes
/// stdin/stdout; tests pass in-memory pipes. ACP sessions map onto Modron
/// sessions lazily: `session/new` returns an opaque id; the first
/// `session/prompt` creates the Modron session, later prompts resume it.
///
/// Approvals: mutating tools in Ask mode raise `session/request_permission`
/// against the editor (allow/deny once) — the safety gate survives the
/// interface change, per PLAN §3.5.
pub(crate) async fn serve<R, W>(
    input: R,
    output: W,
    base: modron_core::Engine,
    launch_dir: PathBuf,
    hub: Arc<AcpHub>,
    mut events: mpsc::UnboundedReceiver<RunEvent>,
) -> anyhow::Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    // Single writer: every emission (responses, errors, updates, permission
    // requests) is queued here in order and written as it arrives.
    let (write_tx, mut write_rx) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        let mut out = tokio::io::BufWriter::new(output);
        while let Some(line) = write_rx.recv().await {
            if out.write_all(line.as_bytes()).await.is_err()
                || out.write_all(b"\n").await.is_err()
                || out.flush().await.is_err()
            {
                break;
            }
        }
    });

    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<Done>();
    let mut server = Server {
        write: write_tx,
        base,
        launch_dir,
        hub,
        sessions: HashMap::new(),
        running: None,
        done: done_tx,
    };
    let mut lines = input.lines();

    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line? else { break };
                if let Some(message) = parse_message(&line) {
                    server.on_message(message);
                }
            }
            Some(event) = events.recv() => server.on_event(event),
            Some(done) = done_rx.recv() => server.on_done(done, &mut events),
        }
    }

    // Input closed: stop any prompt still running, then let the writer drain.
    let _ = server.stop_running();
    drop(server);
    let _ = writer.await;
    Ok(())
}

/// `modron acp`: the agent is built from the same CLI config as every other
/// front end, with the editor as the approver.
pub(crate) async fn run_acp(cli: &Cli, cfg: &config::Config) -> anyhow::Result<()> {
    let mcp = spawn_mcp_servers(cfg).await?;
    let (hub, events) = AcpHub::new();
    let agent = build_agent_with(
        cli,
        cfg,
        true, // the editor is the approver
        hub.sinks(),
        &mcp,
    )?;
    serve(
        tokio::io::BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
        agent.engine,
        cli.working_dir.clone(),
        hub,
        events,
    )
    .await
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
    use modron_core::testing::{InMemoryStore, MockProvider, StaticSelector};
    use modron_core::{
        ApprovalMode, ApprovalPolicy, ModelProvider, StopReason, ToolCall, TurnRequest,
        TurnResponse, Usage,
    };
    use std::future::Future;
    use std::pin::Pin;
    use std::time::{Duration, Instant};
    use tokio::io::{BufReader, DuplexStream, Lines};

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
    fn session_cwd_uses_real_param_dirs_and_falls_back() {
        // A cwd that exists on disk wins; a missing/absent one falls back
        // to the launch dir (never a silently broken working dir).
        let real = tempfile::tempdir().unwrap();
        let params = json!({"cwd": real.path().to_str().unwrap()});
        assert_eq!(
            session_cwd(&params, std::path::Path::new("/launch")),
            real.path()
        );
        assert_eq!(
            session_cwd(
                &json!({"cwd": "/nonexistent"}),
                std::path::Path::new("/launch")
            ),
            std::path::Path::new("/launch")
        );
        assert_eq!(
            session_cwd(&json!({}), std::path::Path::new("/launch")),
            std::path::Path::new("/launch")
        );
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
        let mut tools = ToolCallIds::default();
        let chunk = update_to_line(
            &AcUpdate::Delta(StreamDelta::Text("hello".into())),
            "s1",
            &mut tools,
        )
        .unwrap();
        assert!(chunk.contains("agent_message_chunk"));
        assert!(chunk.contains("hello"));

        let tool = update_to_line(
            &AcUpdate::Tool(ToolActivity::Start {
                name: "bash".into(),
            }),
            "s1",
            &mut tools,
        )
        .unwrap();
        assert!(tool.contains("tool_call"));
        assert!(tool.contains("in_progress"));
    }

    #[test]
    fn tool_completion_reuses_the_start_id() {
        let mut tools = ToolCallIds::default();
        let start: Value = serde_json::from_str(
            &update_to_line(
                &AcUpdate::Tool(ToolActivity::Start {
                    name: "write".into(),
                }),
                "s1",
                &mut tools,
            )
            .unwrap(),
        )
        .unwrap();
        let done: Value = serde_json::from_str(
            &update_to_line(
                &AcUpdate::Tool(ToolActivity::Done {
                    name: "write".into(),
                    is_error: false,
                    bytes: 3,
                }),
                "s1",
                &mut tools,
            )
            .unwrap(),
        )
        .unwrap();
        let start_id = &start["params"]["update"]["toolCallId"];
        assert_eq!(start_id, "call-1");
        assert_eq!(
            done["params"]["update"]["sessionUpdate"],
            "tool_call_update"
        );
        assert_eq!(&done["params"]["update"]["toolCallId"], start_id);
        assert_eq!(done["params"]["update"]["status"], "completed");
    }

    /// A provider whose turn is held open until the test opens the gate. It
    /// can stream one text delta first. A gate that never opens fails the
    /// turn after 10 seconds, so a broken test cannot hang.
    struct HeldProvider {
        text: Option<String>,
        gate: Arc<tokio::sync::Notify>,
    }

    impl HeldProvider {
        async fn finish(&self) -> modron_core::Result<TurnResponse> {
            if tokio::time::timeout(Duration::from_secs(10), self.gate.notified())
                .await
                .is_err()
            {
                return Err(modron_core::Error::Provider {
                    provider: "held".into(),
                    message: "gate never opened".into(),
                });
            }
            Ok(TurnResponse {
                text: self.text.clone().unwrap_or_default(),
                tool_calls: Vec::new(),
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    cached_input_tokens: 0,
                },
                stop_reason: StopReason::EndTurn,
            })
        }
    }

    // Hand-written `#[async_trait]` expansion: modron-cli has no async-trait
    // dependency.
    impl ModelProvider for HeldProvider {
        fn id(&self) -> &str {
            "mock"
        }

        fn turn<'life0, 'async_trait>(
            &'life0 self,
            _request: Arc<TurnRequest>,
        ) -> Pin<Box<dyn Future<Output = modron_core::Result<TurnResponse>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move { self.finish().await })
        }

        fn turn_streaming<'life0, 'async_trait>(
            &'life0 self,
            _request: Arc<TurnRequest>,
            tx: mpsc::UnboundedSender<StreamDelta>,
        ) -> Pin<Box<dyn Future<Output = modron_core::Result<TurnResponse>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move {
                if let Some(text) = &self.text {
                    let _ = tx.send(StreamDelta::Text(text.clone()));
                }
                self.finish().await
            })
        }
    }

    /// An engine on `provider` with the hub's sinks and approve-all tools.
    fn test_engine<P: ModelProvider + 'static>(
        provider: P,
        sinks: AgentSinks,
        tools: modron_core::ToolRegistry,
    ) -> modron_core::Engine {
        let config = modron_core::EngineConfig {
            approver: sinks.approver,
            on_delta: sinks.on_delta,
            on_tool_event: sinks.on_tool_event,
            approvals: ApprovalPolicy {
                mode: ApprovalMode::ApproveAll,
                non_interactive: true,
                ..ApprovalPolicy::default()
            },
            ..modron_core::EngineConfig::default()
        };
        modron_core::Engine {
            selector: Arc::new(StaticSelector::new("mock-model", "mock")),
            providers: [(
                "mock".to_string(),
                Arc::new(provider) as Arc<dyn ModelProvider>,
            )]
            .into_iter()
            .collect(),
            tools: Arc::new(tools),
            store: Arc::new(InMemoryStore::default()),
            config,
        }
    }

    /// The server running on in-memory pipes, driven by the test as the editor.
    struct Harness {
        to_server: DuplexStream,
        from_server: Lines<BufReader<DuplexStream>>,
    }

    fn spawn_server<P: ModelProvider + 'static>(
        provider: P,
        tools: modron_core::ToolRegistry,
        launch_dir: &std::path::Path,
    ) -> Harness {
        use tokio::io::AsyncBufReadExt as _;
        let (hub, events) = AcpHub::new();
        let engine = test_engine(provider, hub.sinks(), tools);
        let (to_server, server_in) = tokio::io::duplex(1 << 16);
        let (server_out, from_server) = tokio::io::duplex(1 << 16);
        // The server task is detached: each test ends by closing its input or cancelling.
        tokio::spawn(serve(
            BufReader::new(server_in),
            server_out,
            engine,
            launch_dir.to_path_buf(),
            hub,
            events,
        ));
        Harness {
            to_server,
            from_server: BufReader::new(from_server).lines(),
        }
    }

    impl Harness {
        async fn send(&mut self, message: Value) {
            let mut line = message.to_string();
            line.push('\n');
            self.to_server.write_all(line.as_bytes()).await.unwrap();
        }

        /// The next message the server writes. Panics after 10 seconds of silence.
        async fn next(&mut self) -> Value {
            let line = tokio::time::timeout(Duration::from_secs(10), self.from_server.next_line())
                .await
                .expect("server went silent")
                .expect("read stdout")
                .expect("server closed stdout");
            serde_json::from_str(&line).expect("valid JSON line")
        }

        /// Messages up to the response to request `id`, which is returned too.
        async fn until_response(&mut self, id: u64) -> (Value, Vec<Value>) {
            let mut seen = Vec::new();
            loop {
                let message = self.next().await;
                if message.get("method").is_none() && message.get("id") == Some(&json!(id)) {
                    return (message, seen);
                }
                seen.push(message);
            }
        }

        async fn new_session(&mut self, cwd: &std::path::Path) -> String {
            self.send(json!({
                "jsonrpc": "2.0", "id": 1, "method": "session/new",
                "params": {"cwd": cwd.to_str().unwrap(), "mcpServers": []}
            }))
            .await;
            let (response, _) = self.until_response(1).await;
            response["result"]["sessionId"]
                .as_str()
                .unwrap()
                .to_string()
        }
    }

    fn prompt(id: u64, session: &str, text: &str) -> Value {
        json!({
            "jsonrpc": "2.0", "id": id, "method": "session/prompt",
            "params": {"sessionId": session, "prompt": [{"type": "text", "text": text}]}
        })
    }

    fn update_kind(message: &Value) -> Option<&str> {
        message
            .pointer("/params/update/sessionUpdate")
            .and_then(Value::as_str)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_stops_a_running_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let gate = Arc::new(tokio::sync::Notify::new());
        let provider = HeldProvider {
            text: None,
            gate: Arc::clone(&gate),
        };
        let mut server = spawn_server(provider, modron_core::ToolRegistry::new(), dir.path());
        let session = server.new_session(dir.path()).await;

        let started = Instant::now();
        server.send(prompt(2, &session, "a long task")).await;
        // Messages are handled in order, so the cancel lands on the running prompt.
        server
            .send(json!({
                "jsonrpc": "2.0", "method": "session/cancel",
                "params": {"sessionId": session}
            }))
            .await;
        let (response, _) = server.until_response(2).await;
        assert_eq!(response["result"]["stopReason"], "cancelled");
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "cancel must not wait for the held turn to time out"
        );
        assert!(
            response.get("error").is_none(),
            "a cancelled prompt is a result, not an error: {response}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tool_call_completion_carries_the_start_id() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new()
            .then_response(TurnResponse {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "write".into(),
                    arguments: json!({"path": "a.txt", "content": "x"}),
                }],
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    cached_input_tokens: 0,
                },
                stop_reason: StopReason::ToolUse,
            })
            .then_text("done");
        let mut tools = modron_core::ToolRegistry::new();
        tools.register(Arc::new(modron_tools::WriteTool));
        let mut server = spawn_server(provider, tools, dir.path());
        let session = server.new_session(dir.path()).await;

        server.send(prompt(2, &session, "write a file")).await;
        let (response, updates) = server.until_response(2).await;
        assert_eq!(response["result"]["stopReason"], "end_turn");

        let starts: Vec<&Value> = updates
            .iter()
            .filter(|u| update_kind(u) == Some("tool_call"))
            .collect();
        let finishes: Vec<&Value> = updates
            .iter()
            .filter(|u| update_kind(u) == Some("tool_call_update"))
            .collect();
        assert_eq!(starts.len(), 1, "one start: {updates:?}");
        assert_eq!(finishes.len(), 1, "one completion: {updates:?}");
        let start_id = &starts[0]["params"]["update"]["toolCallId"];
        assert_eq!(&finishes[0]["params"]["update"]["toolCallId"], start_id);
        assert_eq!(finishes[0]["params"]["update"]["status"], "completed");
        assert!(dir.path().join("a.txt").exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unknown_session_is_a_json_rpc_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut server = spawn_server(
            MockProvider::new().then_text("unused"),
            modron_core::ToolRegistry::new(),
            dir.path(),
        );
        server.send(prompt(7, "no-such-session", "hi")).await;
        let (response, _) = server.until_response(7).await;
        assert_eq!(response["error"]["code"], -32602);
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap()
                .contains("no-such-session"),
            "{response}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_text_is_written_before_the_run_ends() {
        // The provider streams one chunk, then waits for the editor to
        // acknowledge it. If the chunk were held until the run ended, the
        // gate would time out and the prompt would fail instead of end_turn.
        let dir = tempfile::tempdir().unwrap();
        let gate = Arc::new(tokio::sync::Notify::new());
        let provider = HeldProvider {
            text: Some("partial answer".into()),
            gate: Arc::clone(&gate),
        };
        let mut server = spawn_server(provider, modron_core::ToolRegistry::new(), dir.path());
        let session = server.new_session(dir.path()).await;

        server.send(prompt(3, &session, "stream")).await;
        loop {
            let message = server.next().await;
            assert!(
                message.get("id") != Some(&json!(3)) || message.get("method").is_some(),
                "the prompt finished before its text was written: {message}"
            );
            if message
                .pointer("/params/update/content/text")
                .and_then(Value::as_str)
                == Some("partial answer")
            {
                break;
            }
        }
        gate.notify_one();
        let (response, _) = server.until_response(3).await;
        assert_eq!(response["result"]["stopReason"], "end_turn");
    }
}
