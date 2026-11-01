//! modron-daemon — the persistent engine server (Phase 9).
//!
//! `modron daemon start` runs the agent engine as a background process
//! listening on a Unix domain socket (`~/.modron/daemon.sock`). Sessions
//! survive terminal close; `modron chat` and `modron tui` auto-connect as
//! clients when the daemon is running (falling back to in-process when not).
//!
//! Protocol: newline-delimited JSON over the socket. Requests carry a
//! method + params; responses stream as zero or more notification objects
//! followed by a result object with the same `id`.
//!
//!   → {"id":1,"method":"send_task","params":{"task":"list files"}}
//!   ← {"id":1,"notification":{"type":"delta","text":"..."}}
//!   ← {"id":1,"notification":{"type":"tool","name":"bash","status":"done"}}
//!   ← {"id":1,"result":{"session_id":"...","answer":"...","usage":{...}}}
//!
//! Methods: ping | send_task | resume_task | list_sessions | stop

use modron_core::{Engine, EngineConfig, RunSummary, SessionId, StreamDelta, ToolActivity};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

/// The daemon's socket path.
pub fn socket_path() -> anyhow::Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
    Ok(home.join(".modron").join("daemon.sock"))
}

/// The daemon's PID file (for status/stop).
pub fn pid_file() -> anyhow::Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
    Ok(home.join(".modron").join("daemon.pid"))
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct Request {
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Serialize, Clone)]
pub struct Notification {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Response {
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notification: Option<Notification>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

pub struct Daemon {
    engine: Arc<Engine>,
}

impl Daemon {
    pub fn new(engine: Engine) -> Self {
        Self {
            engine: Arc::new(engine),
        }
    }

    /// Run the daemon: bind the socket, accept connections, handle each on
    /// its own task. Writes the PID file; removes the socket on shutdown.
    pub async fn run(&self) -> anyhow::Result<()> {
        let sock = socket_path()?;
        if let Some(parent) = sock.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _ = std::fs::remove_file(&sock); // stale socket from a crash

        let listener = UnixListener::bind(&sock)?;
        std::fs::write(pid_file()?, std::process::id().to_string())?;
        println!(
            "modron daemon listening on {} (pid {})",
            sock.display(),
            std::process::id()
        );

        // Clean shutdown on ctrl+c: remove socket + PID file.
        let sock_for_cleanup = sock.clone();
        tokio::spawn(async move {
            tokio::signal::ctrl_c().await.ok();
            let _ = std::fs::remove_file(&sock_for_cleanup);
            if let Ok(pid) = pid_file() {
                let _ = std::fs::remove_file(&pid);
            };
            std::process::exit(0);
        });

        loop {
            let (stream, _) = listener.accept().await?;
            let engine = Arc::clone(&self.engine);
            tokio::spawn(async move {
                if let Err(e) = handle_connection(stream, engine).await {
                    eprintln!("daemon connection error: {e}");
                }
            });
        }
    }
}

async fn handle_connection(stream: UnixStream, engine: Arc<Engine>) -> anyhow::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let (tx, mut rx) = mpsc::unbounded_channel::<Response>();

    // Writer task: drains the channel to the socket.
    let writer_task = tokio::spawn(async move {
        while let Some(response) = rx.recv().await {
            let mut line = serde_json::to_string(&response)?;
            line.push('\n');
            if writer.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
        Ok::<(), anyhow::Error>(())
    });

    while let Some(line) = lines.next_line().await? {
        let Ok(request) = serde_json::from_str::<Request>(&line) else {
            continue;
        };
        let engine = Arc::clone(&engine);
        let tx = tx.clone();
        tokio::spawn(async move {
            handle_request(request, engine, tx).await;
        });
    }

    drop(tx);
    let _ = writer_task.await;
    Ok(())
}

async fn handle_request(
    request: Request,
    engine: Arc<Engine>,
    tx: mpsc::UnboundedSender<Response>,
) {
    match request.method.as_str() {
        "ping" => {
            let _ = tx.send(Response {
                id: request.id,
                notification: None,
                result: Some(json!({"status": "ok", "version": env!("CARGO_PKG_VERSION")})),
                error: None,
            });
        }
        "send_task" => {
            let task = request
                .params
                .get("task")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            run_task(request.id, engine, Some(task), None, tx).await;
        }
        "resume_task" => {
            let task = request
                .params
                .get("task")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let session = request
                .params
                .get("session_id")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<SessionId>().ok());
            run_task(request.id, engine, Some(task), session, tx).await;
        }
        "list_sessions" => match engine.store.list().await {
            Ok(sessions) => {
                let _ = tx.send(Response {
                    id: request.id,
                    notification: None,
                    result: Some(json!({"sessions": sessions})),
                    error: None,
                });
            }
            Err(e) => {
                let _ = tx.send(Response {
                    id: request.id,
                    notification: None,
                    result: None,
                    error: Some(e.to_string()),
                });
            }
        },
        "stop" => {
            let _ = tx.send(Response {
                id: request.id,
                notification: None,
                result: Some(json!({"stopping": true})),
                error: None,
            });
            if let Ok(sock) = socket_path() {
                let _ = std::fs::remove_file(&sock);
            };
            if let Ok(pid) = pid_file() {
                let _ = std::fs::remove_file(&pid);
            };
            std::process::exit(0);
        }
        other => {
            let _ = tx.send(Response {
                id: request.id,
                notification: None,
                result: None,
                error: Some(format!("unknown method: {other}")),
            });
        }
    }
}

async fn run_task(
    id: u64,
    engine: Arc<Engine>,
    task: Option<String>,
    resume: Option<SessionId>,
    tx: mpsc::UnboundedSender<Response>,
) {
    let Some(task) = task else { return };
    let notify = {
        let tx = tx.clone();
        move |kind: &str, text: Option<String>, name: Option<String>, status: Option<String>| {
            let _ = tx.send(Response {
                id,
                notification: Some(Notification {
                    kind: kind.to_string(),
                    text,
                    name,
                    status,
                }),
                result: None,
                error: None,
            });
        }
    };

    // Wire the engine's sinks to forward deltas and tool activity.
    let (delta_tx, mut delta_rx) = mpsc::unbounded_channel::<StreamDelta>();
    let (tool_tx, mut tool_rx) = mpsc::unbounded_channel::<ToolActivity>();
    let delta_sink: Arc<dyn Fn(StreamDelta) + Send + Sync> = Arc::new(move |d| {
        let _ = delta_tx.send(d);
    });
    let tool_sink: Arc<dyn Fn(ToolActivity) + Send + Sync> = Arc::new(move |t| {
        let _ = tool_tx.send(t);
    });

    // Build a per-task engine sharing the daemon's components but with
    // notification sinks attached.
    let config = EngineConfig {
        on_delta: Some(delta_sink),
        on_tool_event: Some(tool_sink),
        ..engine.config.clone()
    };
    let task_engine = Engine {
        selector: Arc::clone(&engine.selector),
        providers: engine.providers.clone(),
        tools: Arc::clone(&engine.tools),
        store: Arc::clone(&engine.store),
        config,
    };

    // Run in a task so we can drain notifications concurrently.
    let run_handle = tokio::spawn(async move {
        match resume {
            Some(session_id) => task_engine.run_task_resuming(session_id, &task).await,
            None => task_engine.run_task(&task).await,
        }
    });

    // Drain notifications while the task runs.
    let notifier = tokio::spawn(async move {
        loop {
            tokio::select! {
                maybe_delta = delta_rx.recv() => {
                    match maybe_delta {
                        Some(StreamDelta::Text(text)) => {
                            notify("delta", Some(text), None, None);
                        }
                        Some(_) => {}
                        None => break,
                    }
                }
                maybe_tool = tool_rx.recv() => {
                    match maybe_tool {
                        Some(ToolActivity::Start { name }) => {
                            notify("tool", None, Some(name), Some("start".to_string()));
                        }
                        Some(ToolActivity::Done { name, is_error, .. }) => {
                            let status = if is_error { "error" } else { "done" };
                            notify("tool", None, Some(name), Some(status.to_string()));
                        }
                        Some(_) => {}
                        None => break,
                    }
                }
            }
        }
    });
    let _ = notifier.await;

    // Send the final result.
    match run_handle.await {
        Ok(Ok(summary)) => {
            let _ = tx.send(Response {
                id,
                notification: None,
                result: Some(summary_to_json(&summary)),
                error: None,
            });
        }
        Ok(Err(e)) => {
            let _ = tx.send(Response {
                id,
                notification: None,
                result: None,
                error: Some(e.to_string()),
            });
        }
        Err(e) => {
            let _ = tx.send(Response {
                id,
                notification: None,
                result: None,
                error: Some(format!("task panicked: {e}")),
            });
        }
    }
}

fn summary_to_json(summary: &RunSummary) -> Value {
    json!({
        "session_id": summary.session_id.to_string(),
        "answer": summary.answer,
        "turns": summary.turns,
        "tool_calls": summary.tool_calls,
        "usage": {
            "input_tokens": summary.usage.input_tokens,
            "output_tokens": summary.usage.output_tokens,
            "cached_input_tokens": summary.usage.cached_input_tokens,
        },
        "models_used": summary.models_used,
    })
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Connect to the daemon if it's running. Returns None when no daemon.
pub async fn connect() -> anyhow::Result<Option<UnixStream>> {
    let sock = socket_path()?;
    if !sock.exists() {
        return Ok(None);
    }
    match UnixStream::connect(&sock).await {
        Ok(stream) => Ok(Some(stream)),
        Err(_) => {
            // Stale socket.
            let _ = std::fs::remove_file(&sock);
            Ok(None)
        }
    }
}

/// Check if the daemon is running (non-connecting).
pub fn is_running() -> bool {
    match (socket_path(), pid_file()) {
        (Ok(sock), Ok(pid)) => {
            sock.exists()
                && pid.exists()
                && std::fs::read_to_string(&pid)
                    .ok()
                    .and_then(|p| p.trim().parse::<u32>().ok())
                    .map(|p| {
                        // Check the process exists (Unix: signal 0).
                        unsafe { libc::kill(p as i32, 0) == 0 }
                    })
                    .unwrap_or(false)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_is_under_modron_dir() {
        let path = socket_path().unwrap();
        assert!(path.to_string_lossy().contains(".modron"));
        assert!(path.to_string_lossy().contains("daemon.sock"));
    }
}
