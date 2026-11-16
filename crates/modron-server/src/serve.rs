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
use modron_core::{Engine, SessionId};
use tokio::net::TcpListener;
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
    /// Force untrusted handling on the WS listener even when it is bound to
    /// loopback: every hello must present a paired device token (D3).
    pub require_token: bool,
    /// Exact `Origin` header values a browser upgrade may present. Empty by
    /// default: any upgrade that carries an `Origin` header is refused (D3).
    pub allowed_origins: Vec<String>,
    /// Bridge URL (`ws://` or `wss://`) to open remote control against at
    /// start, with no client: see [`crate::rc::start_serve_remote_control`].
    /// `None` = no tunnel (today's behaviour).
    pub remote_control: Option<String>,
    /// File the share URL is written to at start (0600). Requires
    /// `remote_control`.
    pub share_url_file: Option<PathBuf>,
    /// Runs at shutdown (ctrl+c or SIGTERM) for the session that remote
    /// control was started for, after that session's run is interrupted.
    /// Needs `remote_control`. `None` = no hand-off at exit.
    pub handoff_on_exit: Option<HandoffOnExit>,
    /// How long the on-exit hook may take before shutdown goes on without it.
    /// `None` = [`DEFAULT_HANDOFF_TIMEOUT`].
    pub handoff_timeout: Option<Duration>,
}

/// Time the on-exit hand-off may take before `modron serve` exits without it.
pub const DEFAULT_HANDOFF_TIMEOUT: Duration = Duration::from_secs(30);

/// The hand-off run at shutdown: given the session, returns a one-line report
/// for stdout, or an error for stderr. Runs on its own thread.
#[derive(Clone)]
pub struct HandoffOnExit(pub Arc<dyn Fn(SessionId) -> anyhow::Result<String> + Send + Sync>);

impl std::fmt::Debug for HandoffOnExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HandoffOnExit")
    }
}

/// What the on-exit hand-off came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandoffExit {
    Done(String),
    Failed(String),
    TimedOut,
}

/// Runs the hook for `session` on a thread and waits at most `timeout`. A
/// hook still running at the timeout is left behind; the process exits anyway.
pub fn run_handoff_on_exit(
    hook: &HandoffOnExit,
    session: SessionId,
    timeout: Duration,
) -> HandoffExit {
    let (tx, rx) = std::sync::mpsc::channel();
    let hook = hook.clone();
    std::thread::spawn(move || {
        let _ = tx.send((hook.0)(session));
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(line)) => HandoffExit::Done(line),
        Ok(Err(error)) => HandoffExit::Failed(format!("{error:#}")),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => HandoffExit::TimedOut,
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            HandoffExit::Failed("the hand-off thread stopped without a result".to_string())
        }
    }
}

/// Stops the serve session's run, then runs the hand-off. Every outcome is
/// printed; none of them stops the shutdown.
fn handoff_at_exit(server: &BoundServer, hook: &HandoffOnExit, timeout: Duration) {
    let Some(session) = server.serve_session else {
        eprintln!(
            "modron serve: --handoff-on-exit has no remote-control session; nothing handed off"
        );
        return;
    };
    // Settle the log first: the run is aborted, so the export reads what was
    // committed before the stop.
    if let Some(handle) = server.state.hub.handle(&session) {
        handle.interrupt();
    }
    match run_handoff_on_exit(hook, session, timeout) {
        HandoffExit::Done(line) => println!("{line}"),
        HandoffExit::Failed(error) => eprintln!("modron serve: handoff on exit failed: {error}"),
        HandoffExit::TimedOut => eprintln!(
            "modron serve: handoff on exit did not finish within {}s; exiting without it",
            timeout.as_secs()
        ),
    }
}

/// Waits for ctrl+c, or SIGTERM on unix (what a container stop sends).
async fn wait_for_stop() {
    #[cfg(unix)]
    {
        if let Ok(mut term) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            return;
        }
    }
    tokio::signal::ctrl_c().await.ok();
}

/// The addresses a server actually bound (returned by [`bind_server`]).
pub struct BoundServer {
    /// The session remote control was started for (`--remote-control`).
    pub serve_session: Option<SessionId>,
    /// `ws://` endpoint, when the WS listener is enabled.
    pub ws_addr: Option<std::net::SocketAddr>,
    /// Unix-socket path, always bound.
    pub unix_path: Option<PathBuf>,
    /// The shared server state (hub + store + token directory).
    pub state: Arc<ServerState>,
    tasks: Vec<JoinHandle<()>>,
}

impl BoundServer {
    /// Abort every accept loop and every remote-control tunnel (tunnels
    /// redial on their own, so they must be stopped here too). Integration
    /// tests use this to tear down.
    pub fn shutdown(&self) {
        for task in &self.tasks {
            task.abort();
        }
        let tunnels = self
            .state
            .rc_tunnels
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for tunnel in tunnels.values() {
            tunnel.abort();
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
    // Checks that need no listener run first, so a bad flag fails before
    // anything is bound.
    if options.share_url_file.is_some() && options.remote_control.is_none() {
        anyhow::bail!("--share-url-file needs --remote-control (or MODRON_REMOTE_CONTROL)");
    }
    if let Some(bridge) = &options.remote_control {
        crate::rc::check_tunnel_bridge(bridge)?;
        crate::rc::prepare_project_marker(&engine.config.working_dir)?;
    }
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

    // Task records persist under the server's data dir (the parent of the
    // device-token directory, `~/.modron` by default) so a restart keeps
    // each task's worktree. Records whose worktree is gone are dropped.
    let data_dir = match clients_dir.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => pairing::default_modron_dir()?,
    };
    let task_registry = crate::tasks::TaskRegistry::load(data_dir.join(crate::tasks::TASKS_FILE));

    let state = Arc::new(ServerState {
        hub,
        store: broadcasting,
        devices_dir: clients_dir,
        version: env!("CARGO_PKG_VERSION").to_string(),
        working_dir,
        tasks: task_registry,
        bridge_base: options.bridge_base.clone(),
        rc_keys: std::sync::Mutex::new(HashMap::new()),
        rc_tunnels: std::sync::Mutex::new(HashMap::new()),
    });

    let mut tasks = Vec::new();

    // The automations scheduler (2026-11-11 audit fix): the CRUD path
    // (add/remove/list frames) worked, but this loop - the thing that
    // actually FIRES due automations into fresh sessions - was never
    // spawned, so scheduled automations sat stored and inert.
    // normalize() first (2026-11-12 audit fix): a registry without
    // next-due stamps (pre-field version, hand-edited) would otherwise
    // mass-fire every automation the moment the scheduler wakes.
    crate::automations::normalize(&state);
    crate::automations::spawn_scheduler(Arc::clone(&state));

    // WebSocket listener (optional).
    let ws_addr = match options.listen.as_deref() {
        Some("disabled") => None,
        listen => {
            let addr = listen.unwrap_or("127.0.0.1:7699");
            let listener = TcpListener::bind(addr)
                .await
                .with_context(|| format!("cannot bind WS listener on {addr}"))?;
            let bound = listener.local_addr()?;
            // Loopback is trusted unless the operator forces tokens (D3).
            let trusted = bound.ip().is_loopback() && !options.require_token;
            tasks.push(tokio::spawn(ws::run_ws_listener(
                listener,
                Arc::clone(&state),
                trusted,
                Arc::new(options.allowed_origins.clone()),
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
        let listener = unix::bind_private_socket(path)?;
        tasks.push(tokio::spawn(unix::run_unix_listener(
            listener,
            Arc::clone(&state),
        )));
    }

    // Remote control at start, once the listeners are up (same tunnel path
    // as the `remote_control` frame). A failure here stops the accept loops
    // before the error goes back to the caller.
    let mut serve_session = None;
    if let Some(bridge) = &options.remote_control {
        match crate::rc::start_serve_remote_control(
            &state,
            bridge,
            options.share_url_file.as_deref(),
        )
        .await
        {
            Ok((_url, session)) => serve_session = Some(session),
            Err(e) => {
                for task in &tasks {
                    task.abort();
                }
                return Err(e);
            }
        }
    }

    Ok(BoundServer {
        serve_session,
        ws_addr,
        unix_path,
        tasks,
        state,
    })
}

/// `modron serve`: bind, print, run until ctrl+c, clean up.
pub async fn run_serve(engine: Engine, options: &ServeOptions) -> anyhow::Result<()> {
    let modron_dir = pairing::default_modron_dir()?;
    pairing::create_private_dir(&modron_dir)?;
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

    wait_for_stop().await;
    if let Some(hook) = &options.handoff_on_exit {
        let timeout = options.handoff_timeout.unwrap_or(DEFAULT_HANDOFF_TIMEOUT);
        handoff_at_exit(&server, hook, timeout);
    }
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

#[cfg(test)]
mod handoff_exit_tests {
    use super::*;

    fn hook(
        f: impl Fn(SessionId) -> anyhow::Result<String> + Send + Sync + 'static,
    ) -> HandoffOnExit {
        HandoffOnExit(Arc::new(f))
    }

    #[test]
    fn a_finished_hook_reports_its_line() {
        let id = SessionId::nil();
        let out = run_handoff_on_exit(
            &hook(|s| Ok(format!("done {s}"))),
            id,
            Duration::from_secs(5),
        );
        assert_eq!(out, HandoffExit::Done(format!("done {id}")));
    }

    #[test]
    fn a_failing_hook_reports_its_error() {
        let out = run_handoff_on_exit(
            &hook(|_| Err(anyhow::anyhow!("store is read-only"))),
            SessionId::nil(),
            Duration::from_secs(5),
        );
        assert_eq!(out, HandoffExit::Failed("store is read-only".to_string()));
    }

    #[test]
    fn a_slow_hook_is_abandoned_at_the_timeout() {
        let started = std::time::Instant::now();
        let out = run_handoff_on_exit(
            &hook(|_| {
                std::thread::sleep(Duration::from_secs(5));
                Ok(String::new())
            }),
            SessionId::nil(),
            Duration::from_millis(50),
        );
        assert_eq!(out, HandoffExit::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[test]
    fn the_default_timeout_is_thirty_seconds() {
        assert_eq!(DEFAULT_HANDOFF_TIMEOUT, Duration::from_secs(30));
    }
}
