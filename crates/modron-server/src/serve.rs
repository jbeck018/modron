//! `modron serve` — the remote-control server lifecycle (Phase 2/3).
//!
//! [`bind_server`] is the public entry point (also what the integration
//! tests drive): wraps the engine's store in a [`BroadcastingStore`],
//! installs the hub, binds the requested listeners, and spawns their
//! accept loops. [`run_serve`] is the foreground command: pid file,
//! startup line, ctrl+c, cleanup.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use modron_core::Engine;
use tokio::net::{TcpListener, UnixListener};
use tokio::task::JoinHandle;

use crate::hub::{BroadcastingStore, SessionHub};
use crate::pairing;
use crate::{dispatch::ServerState, unix, ws};

/// Listener configuration for [`bind_server`].
#[derive(Clone, Debug, Default)]
pub struct ServeOptions {
    /// WebSocket bind address, or `Some("disabled")` to run with the Unix
    /// socket only. `None` = the default `127.0.0.1:7699`.
    pub listen: Option<String>,
    /// Unix-socket path; `None` = `~/.modron/serve.sock`.
    pub socket: Option<PathBuf>,
    /// Device-token directory; `None` = `~/.modron/clients`.
    pub clients_dir: Option<PathBuf>,
    /// Remote-control bridge base (scheme + host) used by /publish; the
    /// `/remote-control` command takes its own URL.
    pub bridge_base: Option<String>,
}

/// The addresses a server actually bound (returned by [`bind_server`]).
pub struct BoundServer {
    /// `ws://` endpoint, when the WS listener is enabled.
    pub ws_addr: Option<std::net::SocketAddr>,
    /// Unix-socket path, always bound.
    pub unix_path: Option<PathBuf>,
    /// The shared server state (hub + store + token directory).
    pub state: Arc<ServerState>,
    tasks: Vec<JoinHandle<()>>,
}

impl BoundServer {
    /// Abort every accept loop (integration tests use this to tear down).
    pub fn shutdown(&self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Drop for BoundServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Build the hub over the engine and bind all requested listeners. The
/// accept loops run on detached tasks; drop the returned value (or call
/// [`BoundServer::shutdown`]) to stop them.
pub async fn bind_server(engine: Engine, options: &ServeOptions) -> anyhow::Result<BoundServer> {
    let clients_dir = match &options.clients_dir {
        Some(dir) => dir.clone(),
        None => pairing::default_clients_dir()?,
    };

    // The hub wraps the engine's store so every append fans out; the
    // engine template's store IS the broadcasting store (per-session
    // clones share it).
    let hub = SessionHub::empty();
    let broadcasting = Arc::new(BroadcastingStore::new(
        Arc::clone(&engine.store),
        hub.clone(),
    ));
    let mut template = engine;
    let working_dir = template.config.working_dir.clone();
    template.store = Arc::clone(&broadcasting) as Arc<dyn modron_core::SessionStore>;
    hub.set_engine(template);

    let state = Arc::new(ServerState {
        hub,
        store: broadcasting,
        devices_dir: clients_dir,
        version: env!("CARGO_PKG_VERSION").to_string(),
        working_dir,
        tasks: crate::tasks::TaskRegistry::new(),
        bridge_base: options.bridge_base.clone(),
        rc_keys: std::sync::Mutex::new(HashMap::new()),
    });

    let mut tasks = Vec::new();

    // WebSocket listener (optional).
    let ws_addr = match options.listen.as_deref() {
        Some("disabled") => None,
        listen => {
            let addr = listen.unwrap_or("127.0.0.1:7699");
            let listener = TcpListener::bind(addr)
                .await
                .with_context(|| format!("cannot bind WS listener on {addr}"))?;
            let bound = listener.local_addr()?;
            let trusted = bound.ip().is_loopback();
            tasks.push(tokio::spawn(ws::run_ws_listener(
                listener,
                Arc::clone(&state),
                trusted,
            )));
            Some(bound)
        }
    };

    // Unix-socket listener (always on: same frames over NDJSON lines).
    let unix_path = match &options.socket {
        Some(path) => Some(path.clone()),
        None => Some(pairing::default_modron_dir()?.join("serve.sock")),
    };
    if let Some(path) = &unix_path {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let _ = std::fs::remove_file(path); // stale socket from a crash
        let listener = UnixListener::bind(path)
            .with_context(|| format!("cannot bind unix socket {}", path.display()))?;
        tasks.push(tokio::spawn(unix::run_unix_listener(
            listener,
            Arc::clone(&state),
        )));
    }

    Ok(BoundServer {
        ws_addr,
        unix_path,
        tasks,
        state,
    })
}

/// `modron serve`: bind, print, run until ctrl+c, clean up.
pub async fn run_serve(engine: Engine, options: &ServeOptions) -> anyhow::Result<()> {
    let modron_dir = pairing::default_modron_dir()?;
    std::fs::create_dir_all(&modron_dir)?;
    let pid_path = modron_dir.join("serve.pid");
    std::fs::write(&pid_path, std::process::id().to_string())?;

    let server = match bind_server(engine, options).await {
        Ok(server) => server,
        Err(e) => {
            let _ = std::fs::remove_file(&pid_path);
            return Err(e);
        }
    };

    let ws_part = server
        .ws_addr
        .map(|addr| format!("ws://{addr}"))
        .unwrap_or_else(|| "ws disabled".to_string());
    let unix_part = server
        .unix_path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "unix disabled".to_string());
    println!(
        "modron serve listening on {ws_part} and {unix_part} (pid {}) — ctrl+c to stop",
        std::process::id()
    );

    tokio::signal::ctrl_c().await.ok();
    // Give in-flight frames a beat, then clean up.
    tokio::time::sleep(Duration::from_millis(50)).await;
    server.shutdown();
    if let Some(path) = &server.unix_path {
        let _ = std::fs::remove_file(path);
    }
    let _ = std::fs::remove_file(&pid_path);
    println!("modron serve stopped");
    Ok(())
}
