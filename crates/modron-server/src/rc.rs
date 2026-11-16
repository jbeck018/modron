//! rc.rs - remote-control tunnels (the `/remote-control` flow): one
//! outbound WebSocket from this server to a relay bridge (a deployed
//! cloud/bridge worker), keyed by a share code. The relay is
//! transport-only for live traffic: it relays Frames and keeps only a
//! bounded replay buffer per session. Published pages are kept by the
//! relay for a limited time (see cloud/bridge/README.md).
//!
//! Over the tunnel this server acts as a normal protocol connection: the
//! web/phone page on the other side of the relay sends ClientMsg frames
//! (hello, attach, submit, approve...) which flow through the SAME
//! `handle_frame` dispatch as every other client, and ServerMsg frames
//! (snapshot, deltas, tool events, approvals) flow back out. A dropped or
//! unreachable socket is redialled with backoff (see [`ReconnectTiming`]),
//! with the same share key, until the tunnel is stopped or the bridge refuses
//! the host token.
//!
//! Trust model: possession of the share key is the credential (like
//! Claude Code share links). Tunnel frames are trusted because the key
//! was minted by this server.

use crate::dispatch::Outbound;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use futures::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::dispatch::{handle_frame, handle_relayed_frame, ConnCtx, Flow, ServerState};
use modron_core::SessionId;

/// The key that goes into a share link: a one-way hash of the agent's
/// secret key. The bridge derives the same value from the agent's
/// connection, so both sides meet in one relay session, but someone holding
/// a share link cannot recover the agent key and connect as the agent.
/// Must match `viewerKeyOf` in cloud/bridge/src/index.ts.
pub fn viewer_key(agent_key: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("modron-viewer:{agent_key}").as_bytes());
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

/// Build the relay URLs for a bridge base + agent key: the agent tunnel
/// `ws(s)://host/agent/{key}` and the viewer page
/// `http(s)://host/s/{viewer key}` (see [`viewer_key`]; the share link
/// never contains the agent key). The viewer page is opened in a browser,
/// so it always carries an http(s) scheme, whatever scheme the bridge
/// base was given with.
pub fn tunnel_urls(bridge_base: &str, key: &str) -> (String, String) {
    let base = bridge_base.trim_end_matches('/');
    let ws_scheme = if base.starts_with("wss://") || base.starts_with("https://") {
        "wss"
    } else {
        "ws"
    };
    let host = base
        .strip_prefix("wss://")
        .or_else(|| base.strip_prefix("ws://"))
        .or_else(|| base.strip_prefix("https://"))
        .or_else(|| base.strip_prefix("http://"))
        .unwrap_or(base);
    let page_scheme = if ws_scheme == "wss" { "https" } else { "http" };
    (
        format!("{ws_scheme}://{host}/agent/{key}"),
        format!("{page_scheme}://{host}/s/{}", viewer_key(key)),
    )
}

/// Is `host` (no scheme, port or userinfo; IPv6 without brackets) a loopback
/// name: `localhost`, `127.0.0.0/8`, or `::1`?
fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    if let Ok(v4) = host.parse::<std::net::Ipv4Addr>() {
        return v4.is_loopback();
    }
    host.parse::<std::net::Ipv6Addr>()
        .is_ok_and(|v6| v6.is_loopback())
}

/// The host part of a `scheme://authority/...` URL (`rest` is what follows
/// the scheme). Strips userinfo, the port, and IPv6 brackets.
fn url_host(rest: &str) -> &str {
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority.rsplit('@').next().unwrap_or("");
    if let Some(bracketed) = host_port.strip_prefix('[') {
        return bracketed.split(']').next().unwrap_or("");
    }
    host_port.split(':').next().unwrap_or("")
}

/// Check that a bridge URL may carry tunnel or publish traffic. `wss://` and
/// `https://` are accepted for any host. Plaintext `ws://` and `http://` are
/// accepted only for a loopback host: the tunnel key, the publish token and
/// the transcript would otherwise cross the network in clear.
pub fn check_bridge_transport(bridge_url: &str) -> Result<()> {
    if bridge_url.starts_with("wss://") || bridge_url.starts_with("https://") {
        return Ok(());
    }
    let plaintext = [("ws://", "wss"), ("http://", "https")]
        .into_iter()
        .find_map(|(prefix, secure)| bridge_url.strip_prefix(prefix).map(|rest| (rest, secure)));
    let Some((rest, secure)) = plaintext else {
        return Err(anyhow!("bridge url must start with ws:// or wss://"));
    };
    if is_loopback_host(url_host(rest)) {
        return Ok(());
    }
    let scheme = bridge_url.split("://").next().unwrap_or("ws");
    Err(anyhow!(
        "refusing a plaintext {scheme}:// bridge on a non-loopback host; use {secure}://"
    ))
}

/// Generate a share key: the full 32 lowercase hex chars of a v4 UUID
/// (122 random bits, D4). Uuid-derived, so no new dependencies.
pub fn share_key() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Environment variable holding the portal host token (the bridge's
/// `HOST_TOKEN` secret). Unset or empty: the tunnel connects exactly as
/// before and registers nothing with the portal.
pub const HOST_TOKEN_ENV: &str = "MODRON_HOST_TOKEN";

/// Environment variable that turns the portal credentials overlay on:
/// `modron serve --portal-credentials` sets it, and so does the cloud
/// container. The tunnel refetches the keys after each reconnect only when
/// it is set (see `dial_once`).
pub const PORTAL_CREDENTIALS_ENV: &str = "MODRON_PORTAL_CREDENTIALS";

/// Whether the portal credentials overlay is on, per `lookup` (`1` or `true`).
pub fn portal_credentials_on(lookup: &dyn Fn(&str) -> Option<String>) -> bool {
    matches!(
        lookup(PORTAL_CREDENTIALS_ENV).as_deref(),
        Some("1" | "true")
    )
}

/// The start-up rule for `--portal-credentials` (PLAN 10.9): the keys come
/// from the bridge the tunnel dials, so it needs the remote-control bridge,
/// and the bridge's HOST_TOKEN for the bearer. `Ok` when the flag is off.
pub fn check_portal_credentials(
    portal: bool,
    remote: Option<&str>,
    host_token: Option<&str>,
) -> Result<()> {
    if !portal {
        return Ok(());
    }
    if remote.is_none() {
        return Err(anyhow!(
            "--portal-credentials needs --remote-control (or MODRON_REMOTE_CONTROL): \
             the keys come from the bridge the tunnel dials"
        ));
    }
    if host_token.filter(|t| !t.is_empty()).is_none() {
        return Err(anyhow!(
            "--portal-credentials needs {HOST_TOKEN_ENV} (the bridge's HOST_TOKEN)"
        ));
    }
    Ok(())
}

/// Environment variable names the portal may send: upper-case letters,
/// digits and underscores, not starting with a digit.
fn valid_env_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

/// Fetches the owner's provider keys from the bridge (`GET
/// /api/host/credentials`, bearer `host_token`) and replaces `overlay` with
/// them. Returns the environment variable names now held. The bridge base is
/// checked as the tunnel's is (`wss://`, or `ws://` on loopback only). On any
/// failure the overlay is left as it was. No value is ever in an error.
pub async fn refresh_portal_credentials(
    bridge_base: &str,
    host_token: &str,
    overlay: &modron_core::credential_overlay::CredentialOverlay,
) -> Result<Vec<String>> {
    check_tunnel_bridge(bridge_base)?;
    let url = format!("{}/api/host/credentials", bridge_http_base(bridge_base)?);
    let client = reqwest::Client::builder()
        .timeout(REGISTRATION_TIMEOUT)
        .build()
        .context("building the credentials client")?;
    let response = client
        .get(&url)
        .header("authorization", format!("Bearer {host_token}"))
        .send()
        .await
        .context("fetching the portal credentials")?;
    let status = response.status();
    if !status.is_success() {
        return Err(anyhow!(
            "the bridge answered {status} to the credentials fetch"
        ));
    }
    let body = response
        .text()
        .await
        .context("reading the portal credentials")?;
    let parsed: serde_json::Value =
        serde_json::from_str(&body).map_err(|_| anyhow!("the credentials answer is not JSON"))?;
    let map = parsed
        .get("credentials")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| anyhow!("the credentials answer has no credentials object"))?;
    let mut values = std::collections::BTreeMap::new();
    for (name, value) in map {
        if !valid_env_name(name) {
            continue;
        }
        if let Some(text) = value.as_str().filter(|t| !t.is_empty()) {
            values.insert(name.clone(), text.to_string());
        }
    }
    Ok(overlay.replace(values))
}

/// [`refresh_portal_credentials`] into the process overlay, for the serve
/// path (start and each reconnect). Logs the environment variable names
/// fetched, never a value; on failure logs a warning and keeps the previous set.
pub async fn refresh_portal_credentials_logged(bridge_base: &str, host_token: &str) {
    let overlay = modron_core::credential_overlay::shared();
    match refresh_portal_credentials(bridge_base, host_token, &overlay).await {
        Ok(names) => {
            tracing::info!(env_vars = ?names, "portal credentials applied to future turns")
        }
        Err(e) => tracing::warn!(
            error = format!("{e:#}"),
            "portal credentials not refreshed; keeping the previous set"
        ),
    }
}

/// Environment variable naming this host's kind in the portal directory:
/// `cloud`; `local` when unset.
pub const HOST_KIND_ENV: &str = "MODRON_HOST_KIND";

/// The cloud run this process serves (PLAN 10.10), set once at start by
/// `modron serve --run-id`. One serve process is one run, so a process-wide
/// value is the right scope: every portal registration it makes carries it.
static SERVE_RUN_ID: OnceLock<String> = OnceLock::new();

/// A run id as the bridge accepts it: [a-z0-9-], 1 to 64 characters.
pub fn check_run_id(value: &str) -> Result<()> {
    let ok = !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(anyhow!("run id must match [a-z0-9-]{{1,64}}"))
    }
}

/// Records the run this process serves. Refused when a different id is
/// already set; repeating the same id is a no-op.
pub fn set_serve_run_id(value: &str) -> Result<()> {
    check_run_id(value)?;
    if SERVE_RUN_ID.set(value.to_string()).is_err()
        && SERVE_RUN_ID.get().map(String::as_str) != Some(value)
    {
        return Err(anyhow!(
            "a different run id is already set for this process"
        ));
    }
    Ok(())
}

fn serve_run_id() -> Option<String> {
    SERVE_RUN_ID.get().cloned()
}

/// Environment lookup, injected so tests never mutate the process env.
pub type EnvLookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// The process environment as an [`EnvLookup`].
pub fn process_env() -> EnvLookup {
    Arc::new(|name| std::env::var(name).ok())
}

/// A variable through `lookup`; empty counts as unset.
fn env_value(lookup: &dyn Fn(&str) -> Option<String>, name: &str) -> Option<String> {
    lookup(name).filter(|value| !value.is_empty())
}

/// Timing of the portal registration. Production uses [`Default`]; tests
/// shorten it so they never sleep for real.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegistrationTiming {
    /// Minimum gap between two registration PUTs for one session.
    pub debounce: Duration,
    /// Re-PUT interval while the tunnel is up, even with no state change.
    pub heartbeat: Duration,
    /// How often the registration task samples the session's running state.
    pub poll: Duration,
}

impl Default for RegistrationTiming {
    fn default() -> Self {
        Self {
            debounce: Duration::from_secs(2),
            heartbeat: Duration::from_secs(60),
            poll: Duration::from_millis(250),
        }
    }
}

/// Per-request timeout for portal calls, so a stalled bridge cannot hold
/// the DELETE at tunnel close for long.
const REGISTRATION_TIMEOUT: Duration = Duration::from_secs(10);

/// The tunnel's WebSocket handshake request. With a host token the
/// `Authorization: Bearer` header is set; the token never appears in an
/// error message.
fn tunnel_request(
    agent_url: &str,
    host_token: Option<&str>,
) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request> {
    let mut request = agent_url
        .into_client_request()
        .map_err(|_| anyhow!("the bridge agent url is malformed"))?;
    if let Some(token) = host_token {
        let value = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| anyhow!("{HOST_TOKEN_ENV} is not a valid header value"))?;
        request.headers_mut().insert(AUTHORIZATION, value);
    }
    Ok(request)
}

/// The http(s) base of the bridge's HTTP routes, derived from the bridge
/// base: `wss://` and `https://` map to `https://`, the rest to `http://`.
fn bridge_http_base(bridge_base: &str) -> Result<String> {
    let base = bridge_base.trim_end_matches('/');
    let host = base
        .strip_prefix("wss://")
        .or_else(|| base.strip_prefix("ws://"))
        .or_else(|| base.strip_prefix("https://"))
        .or_else(|| base.strip_prefix("http://"))
        .ok_or_else(|| anyhow!("bridge url must start with ws:// or wss://"))?;
    let http = if base.starts_with("wss://") || base.starts_with("https://") {
        "https"
    } else {
        "http"
    };
    Ok(format!("{http}://{host}"))
}

/// Trim control characters to spaces, trim, and keep at most `max_chars`.
fn clean_text(text: &str, max_chars: usize) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .chars()
        .take(max_chars)
        .collect()
}

/// The machine name for the portal directory: `HOSTNAME`, else
/// `/etc/hostname`, else `modron`.
fn host_label(lookup: &dyn Fn(&str) -> Option<String>) -> String {
    let raw = env_value(lookup, "HOSTNAME")
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .unwrap_or_default();
    let label = clean_text(&raw, 100);
    if label.is_empty() {
        "modron".to_string()
    } else {
        label
    }
}

/// The session's title for the portal, or `session <short id>` when it has
/// none (at most 200 characters).
async fn session_title(state: &ServerState, session: SessionId) -> String {
    let stored = state.store.list().await.ok().and_then(|metas| {
        metas
            .into_iter()
            .find(|meta| meta.id == session)
            .map(|meta| meta.title)
    });
    let title = clean_text(&stored.unwrap_or_default(), 200);
    if title.is_empty() {
        let short = session.simple().to_string();
        clean_text(&format!("session {}", &short[..8]), 200)
    } else {
        title
    }
}

/// What a registration task needs to speak to the portal for one session.
struct PortalHost {
    /// The bridge's http(s) base (see [`bridge_http_base`]).
    http: String,
    session: SessionId,
    /// The viewer key the bridge derives from the agent key.
    viewer_key: String,
    token: String,
    host_label: String,
    host_kind: &'static str,
    /// The cloud run this session belongs to (see [`set_serve_run_id`]).
    run_id: Option<String>,
    state: Arc<ServerState>,
    timing: RegistrationTiming,
}

impl PortalHost {
    fn url(&self) -> String {
        format!("{}/api/host/sessions/{}", self.http, self.session)
    }

    /// Busy = a run is in the session's slot.
    fn running(&self) -> bool {
        self.state
            .hub
            .handle(&self.session)
            .is_some_and(|handle| handle.is_busy())
    }
}

/// The portal's answer to one registration request.
enum PutOutcome {
    /// Accepted (2xx): the directory entry is current.
    Accepted,
    /// 503: the bridge has no portal. Registration is off for this tunnel.
    NotConfigured,
    /// 401 or 403: the host token was refused. Never retried.
    Rejected,
    /// Anything else: retried on the next heartbeat.
    Failed,
}

/// A running registration task. Dropping `stop` (the tunnel ended, or its
/// task was aborted) makes the task DELETE the session entry and exit.
struct Registration {
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl Registration {
    /// Stop the task and wait for its DELETE.
    async fn finish(mut self) {
        drop(self.stop.take());
        let _ = self.task.await;
    }
}

/// Start registering the session with the portal (the host token is set).
/// Runs on its own task so it never blocks the tunnel.
fn start_registration(
    lookup: &dyn Fn(&str) -> Option<String>,
    timing: RegistrationTiming,
    state: Arc<ServerState>,
    session: SessionId,
    bridge_base: &str,
    key: &str,
    token: String,
) -> Option<Registration> {
    let Ok(http) = bridge_http_base(bridge_base) else {
        return None;
    };
    let host = PortalHost {
        http,
        session,
        viewer_key: viewer_key(key),
        token,
        host_label: host_label(lookup),
        run_id: serve_run_id(),
        host_kind: if env_value(lookup, HOST_KIND_ENV).as_deref() == Some("cloud") {
            "cloud"
        } else {
            "local"
        },
        state,
        timing,
    };
    let (stop_tx, stop_rx) = oneshot::channel();
    let task = tokio::spawn(registration_loop(host, stop_rx));
    Some(Registration {
        stop: Some(stop_tx),
        task,
    })
}

/// The registration schedule. A PUT goes out on the first tick, when the
/// running state changed (at most one per `debounce`), and on every
/// `heartbeat`. A 503 or 401/403 turns the task off for good; any other
/// failure waits for the next heartbeat. When `stop` closes, the entry is
/// DELETEd unless the portal already refused this tunnel.
async fn registration_loop(host: PortalHost, mut stop: oneshot::Receiver<()>) {
    let client = reqwest::Client::new();
    let url = host.url();
    let mut last_attempt: Option<Instant> = None;
    let mut sent_running: Option<bool> = None;
    let mut off = false;
    let mut ticker = tokio::time::interval(host.timing.poll);
    loop {
        if !off {
            let running = host.running();
            let due = match last_attempt {
                None => true,
                Some(at) => {
                    let elapsed = at.elapsed();
                    (sent_running != Some(running) && elapsed >= host.timing.debounce)
                        || elapsed >= host.timing.heartbeat
                }
            };
            if due {
                match put_entry(&client, &url, &host, running).await {
                    PutOutcome::Accepted => {}
                    PutOutcome::NotConfigured => {
                        tracing::debug!(session = %host.session, "portal not configured; not registering");
                        off = true;
                    }
                    PutOutcome::Rejected => {
                        tracing::warn!(session = %host.session, "portal rejected the host token");
                        off = true;
                    }
                    PutOutcome::Failed => {
                        tracing::debug!(session = %host.session, "portal registration failed; retrying on the next heartbeat");
                    }
                }
                last_attempt = Some(Instant::now());
                sent_running = Some(running);
            }
        }
        tokio::select! {
            _ = &mut stop => break,
            _ = ticker.tick() => {}
        }
    }
    if !off {
        let outcome = client
            .delete(&url)
            .header(AUTHORIZATION, format!("Bearer {}", host.token))
            .timeout(REGISTRATION_TIMEOUT)
            .send()
            .await;
        if !matches!(outcome, Ok(ref r) if r.status().is_success()) {
            tracing::debug!(session = %host.session, "portal entry removal failed");
        }
    }
}

/// The JSON body of one directory PUT. `run_id` is included only for a cloud
/// run's session (the bridge moves the run to running on it).
fn entry_body(
    title: &str,
    host_label: &str,
    host_kind: &str,
    viewer_key: &str,
    run_id: Option<&str>,
    running: bool,
) -> serde_json::Value {
    let mut body = serde_json::json!({
        "title": title,
        "host_label": host_label,
        "host_kind": host_kind,
        "viewer_key": viewer_key,
        "state": if running { "running" } else { "idle" },
    });
    if let Some(run) = run_id {
        body["run_id"] = serde_json::Value::String(run.to_string());
    }
    body
}

/// One PUT of the session's directory entry.
async fn put_entry(
    client: &reqwest::Client,
    url: &str,
    host: &PortalHost,
    running: bool,
) -> PutOutcome {
    let title = session_title(&host.state, host.session).await;
    let body = entry_body(
        &title,
        &host.host_label,
        host.host_kind,
        &host.viewer_key,
        host.run_id.as_deref(),
        running,
    );
    let response = client
        .put(url)
        .header(AUTHORIZATION, format!("Bearer {}", host.token))
        .header("content-type", "application/json")
        .timeout(REGISTRATION_TIMEOUT)
        .body(body.to_string())
        .send()
        .await;
    match response.map(|r| r.status().as_u16()) {
        Ok(status) if (200..300).contains(&status) => PutOutcome::Accepted,
        Ok(503) => PutOutcome::NotConfigured,
        Ok(401 | 403) => PutOutcome::Rejected,
        _ => PutOutcome::Failed,
    }
}

/// Redial schedule of a tunnel. Production uses [`Default`]; tests shorten
/// it, the same injected-clock pattern as [`RegistrationTiming`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReconnectTiming {
    /// The first redial delay, before jitter.
    pub initial: Duration,
    /// The longest nominal delay between two dials.
    pub cap: Duration,
    /// A connection that stayed up this long starts the schedule over.
    pub stable: Duration,
    /// While redials sit at the cap, at most one log line per this interval.
    pub log_interval: Duration,
}

impl Default for ReconnectTiming {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(1),
            cap: Duration::from_secs(60),
            stable: Duration::from_secs(30),
            log_interval: Duration::from_secs(60),
        }
    }
}

/// The delay before the redial that follows `failures` consecutive failures
/// (0 = the first redial): `initial` doubled per failure, capped at `cap`,
/// then jittered into `[nominal / 2, nominal]` by `jitter` (0.0 to 1.0).
pub fn backoff_delay(timing: &ReconnectTiming, failures: u32, jitter: f64) -> Duration {
    let factor = 2u32.checked_pow(failures).unwrap_or(u32::MAX);
    let nominal = timing
        .initial
        .checked_mul(factor)
        .unwrap_or(timing.cap)
        .min(timing.cap);
    let jitter = if jitter.is_finite() {
        jitter.clamp(0.0, 1.0)
    } else {
        0.0
    };
    nominal.mul_f64(0.5 + 0.5 * jitter)
}

/// The failure count to use after a dial that was up for `up_for`: a
/// connection that stayed up for `stable` starts the schedule over.
pub fn schedule_after(failures: u32, up_for: Duration, timing: &ReconnectTiming) -> u32 {
    if up_for >= timing.stable {
        0
    } else {
        failures
    }
}

/// Whether a redial log line is due. Below the cap every attempt logs; at
/// the cap at most one line per `log_interval` (`since_last` is the time
/// since the previous line, `None` when there was none).
pub fn redial_log_due(
    delay: Duration,
    timing: &ReconnectTiming,
    since_last: Option<Duration>,
) -> bool {
    if delay < timing.cap {
        return true;
    }
    match since_last {
        None => true,
        Some(since) => since >= timing.log_interval,
    }
}

/// A random jitter in `[0.0, 1.0]` from the UUID generator (no new dependency).
fn jitter_unit() -> f64 {
    uuid::Uuid::new_v4().as_u128() as f64 / u128::MAX as f64
}

/// The message for a bridge that refused the tunnel's handshake for good.
/// Names the fix and never a token value.
pub fn refusal_message(status: u16) -> String {
    format!(
        "the bridge refused the remote-control tunnel ({status}): the host token is missing or \
         wrong; set {HOST_TOKEN_ENV} to the bridge's HOST_TOKEN secret and start remote control \
         again (it is not retried)"
    )
}

/// A failed dial, with the share key and the host token redacted.
pub fn connect_failure_text(
    bridge_base: &str,
    key: &str,
    host_token: Option<&str>,
    error: &WsError,
) -> String {
    // The transport error's own chain is the useful part; the share key and
    // the host token must never reach a log line.
    let cause = redact(&error_chain(error), &[key, host_token.unwrap_or("")]);
    format!("connecting to the remote-control bridge at {bridge_base}: {cause}")
}

/// Where a tunnel dials, fixed for its whole life (every redial reuses the
/// same share key, so existing share links and portal entries keep working).
struct TunnelTarget {
    session: SessionId,
    bridge_base: String,
    key: String,
    agent_url: String,
    host_token: Option<String>,
    /// Refetch the portal credentials after each connect (`--portal-credentials`).
    portal: bool,
}

/// How one dial of the tunnel ended.
enum Dial {
    /// The socket was up for `up_for`, then closed or failed.
    Ended { up_for: Duration },
    /// The handshake was refused for good (401/403): never redialled.
    Refused(u16),
    /// The dial itself failed (unreachable, or a handshake error): redialled.
    Failed(String),
}

/// True while `key` is still the share key minted for `session`. Ends a
/// tunnel whose session was deleted (its key is forgotten) or whose remote
/// control was restarted with a new key.
fn key_is_current(state: &ServerState, session: SessionId, key: &str) -> bool {
    state
        .rc_keys
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&session)
        .is_some_and(|current| current == key)
}

/// Run the tunnel for one session until it is stopped. Frames from the relay
/// are dispatched exactly like local client frames; outbound frames
/// (snapshot, deltas, events) are relayed to the bridge. With
/// `MODRON_HOST_TOKEN` set, the session is also registered in the portal
/// directory for as long as each connection lives.
pub async fn run_tunnel(
    state: Arc<ServerState>,
    session: SessionId,
    bridge_base: String,
    key: String,
) -> Result<()> {
    run_tunnel_with(
        process_env(),
        RegistrationTiming::default(),
        state,
        session,
        bridge_base,
        key,
    )
    .await
}

/// [`run_tunnel`] with the environment and the registration timing
/// injected (tests use this); the redial schedule is the default one.
pub async fn run_tunnel_with(
    lookup: EnvLookup,
    timing: RegistrationTiming,
    state: Arc<ServerState>,
    session: SessionId,
    bridge_base: String,
    key: String,
) -> Result<()> {
    run_tunnel_timed(
        lookup,
        timing,
        ReconnectTiming::default(),
        state,
        session,
        bridge_base,
        key,
    )
    .await
}

/// The tunnel supervisor. Dials the bridge, serves the connection until it
/// closes, and redials with exponential backoff and jitter (see
/// [`ReconnectTiming`]). The same share key and session are reused on every
/// dial, and each connection re-registers the portal entry and sends a fresh
/// snapshot to its viewers (the handshake's Attach). It returns `Ok` when the
/// tunnel is stopped (the session's key was forgotten; a stopped tunnel task
/// is aborted, never returns). It returns `Err` only when the bridge refuses
/// the host token (401/403) or the request cannot be built; neither is
/// retried. A stop is an abort of the task (see [`start_remote_control`]).
pub async fn run_tunnel_timed(
    lookup: EnvLookup,
    timing: RegistrationTiming,
    reconnect: ReconnectTiming,
    state: Arc<ServerState>,
    session: SessionId,
    bridge_base: String,
    key: String,
) -> Result<()> {
    // The agent URL carries the share key: never put it in an error
    // message or a log line, only the bridge base.
    let (agent_url, _open_url) = tunnel_urls(&bridge_base, &key);
    let host_token = env_value(&*lookup, HOST_TOKEN_ENV);
    // A bad header is a configuration error: checked once, never redialled.
    tunnel_request(&agent_url, host_token.as_deref())
        .with_context(|| format!("preparing the remote-control bridge request at {bridge_base}"))?;
    let portal = portal_credentials_on(&*lookup) && host_token.is_some();
    let target = TunnelTarget {
        session,
        bridge_base,
        key,
        agent_url,
        host_token,
        portal,
    };
    let mut failures = 0u32;
    let mut last_logged: Option<Instant> = None;
    loop {
        if !key_is_current(&state, session, &target.key) {
            tracing::info!(%session, "remote-control tunnel stopped");
            return Ok(());
        }
        let delay = match dial_once(&target, &lookup, timing, &state).await {
            Dial::Refused(status) => return Err(anyhow!(refusal_message(status))),
            Dial::Ended { up_for } => {
                failures = schedule_after(failures, up_for, &reconnect);
                let delay = backoff_delay(&reconnect, failures, jitter_unit());
                tracing::info!(%session, ?delay, "remote-control tunnel dropped; redialling");
                delay
            }
            Dial::Failed(cause) => {
                let delay = backoff_delay(&reconnect, failures, jitter_unit());
                let since = last_logged.map(|at| at.elapsed());
                if redial_log_due(delay, &reconnect, since) {
                    tracing::info!(
                        %session,
                        attempt = failures + 1,
                        ?delay,
                        error = %cause,
                        "remote-control bridge unreachable; retrying"
                    );
                    last_logged = Some(Instant::now());
                }
                delay
            }
        };
        failures = failures.saturating_add(1);
        tokio::time::sleep(delay).await;
    }
}

/// One dial: connect, serve the connection until it ends, and report how it
/// went.
async fn dial_once(
    target: &TunnelTarget,
    lookup: &EnvLookup,
    timing: RegistrationTiming,
    state: &Arc<ServerState>,
) -> Dial {
    let session = target.session;
    let request = match tunnel_request(&target.agent_url, target.host_token.as_deref()) {
        Ok(request) => request,
        Err(e) => return Dial::Failed(format!("{e:#}")),
    };
    let socket = match connect_async(request).await {
        Ok((socket, _)) => socket,
        Err(WsError::Http(response)) if matches!(response.status().as_u16(), 401 | 403) => {
            return Dial::Refused(response.status().as_u16());
        }
        Err(e) => {
            return Dial::Failed(connect_failure_text(
                &target.bridge_base,
                &target.key,
                target.host_token.as_deref(),
                &e,
            ))
        }
    };
    let up_since = Instant::now();
    tracing::info!(%session, "remote-control tunnel connected");
    // Portal credentials refreshed on every connect, before any frame flows.
    if target.portal {
        if let Some(token) = target.host_token.as_deref() {
            refresh_portal_credentials_logged(&target.bridge_base, token).await;
        }
    }
    let (mut sink, mut stream) = socket.split();

    // Server->relay writer: drains the connection's outbound frames (the
    // dispatch pipeline pushes every reply here via ConnCtx).
    let (out_tx, mut out_rx) = mpsc::unbounded_channel();
    // The tunnel is authenticated by construction (its key was minted by
    // this server) and pinned to its one session (D5): the relay's frames
    // get the scoped allowlist, never the server-wide one.
    // With the host token the bridge's relayed `role` is trusted (see
    // `ConnCtx::trusted_tunnel`); without it this is the legacy share-link
    // relay and the role field is ignored.
    let trusted = target.host_token.is_some();
    let mut ctx = if trusted {
        ConnCtx::trusted_tunnel(out_tx, session)
    } else {
        ConnCtx::scoped(out_tx, session)
    };

    // Handshake: these two frames are requests TO this server, on behalf of
    // the tunnel, so they go through the same dispatch as every relayed frame
    // (once the writer is draining). The tunnel's connection is subscribed to
    // its session and the snapshot and every live frame flow to the bridge.
    // Writing them into the sink instead would send them to the viewers as if
    // the agent had said them, and subscribe nothing.
    let hello = modron_protocol::Frame::request(
        0,
        modron_protocol::Payload::Hello {
            client: "modron-remote-control".into(),
            protocol: modron_protocol::PROTOCOL_VERSION,
            token: None,
        },
    );
    let attach = modron_protocol::Frame::request(
        1,
        modron_protocol::Payload::Attach {
            session_id: session,
            last_seq: 0,
        },
    );

    let writer = tokio::spawn(async move {
        while let Some(outbound) = out_rx.recv().await {
            let text = match outbound {
                Outbound::Reply(frame) => serde_json::to_string(&frame),
                // Broadcasts arrive pre-serialized (one JSON build shared
                // by every viewer).
                Outbound::Live(live) => Ok(live.json.to_string()),
            };
            let Ok(text) = text else { break };
            if sink.send(WsMessage::Text(text.into())).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    for frame in [hello, attach] {
        if handle_frame(state, &mut ctx, true, frame).await == Flow::Close {
            drop(ctx);
            let _ = writer.await;
            return Dial::Ended {
                up_for: up_since.elapsed(),
            };
        }
    }

    // Portal registration, only with a host token (see HOST_TOKEN_ENV). It
    // lives with this connection: the entry is DELETEd before a redial and
    // PUT again by the next one, so the directory always matches the socket.
    let registration = target.host_token.clone().and_then(|token| {
        start_registration(
            &**lookup,
            timing,
            Arc::clone(state),
            session,
            &target.bridge_base,
            &target.key,
            token,
        )
    });

    // Relay loop: every frame from the bridge goes through the SAME
    // dispatch as local clients, under the tunnel's scope (see ConnCtx::scoped).
    loop {
        let relayed = tokio::select! {
            frame = stream.next() => match frame {
                Some(Ok(WsMessage::Text(text))) => serde_json::from_str::<
                    modron_protocol::Frame,
                >(&text)
                .ok(),
                _ => break,
            },
            _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
                // The keepalive is the server's own frame, not a relayed one.
                let ping = modron_protocol::Frame::request(
                    0,
                    modron_protocol::Payload::Ping {},
                );
                if handle_frame(state, &mut ctx, true, ping).await == Flow::Close {
                    break;
                }
                continue;
            }
        };
        let Some(frame) = relayed else {
            continue;
        };
        // A viewer that just joined says hello and may not know the session
        // id (share links carry only the viewer key). Answer every relayed
        // hello with a fresh snapshot of the tunnel's one session, so a
        // late viewer gets full history from the server instead of relying
        // on what the relay happened to buffer. A snapshot resets a client's
        // view, so viewers already attached are unaffected.
        let joined = matches!(frame.payload, modron_protocol::Payload::Hello { .. });
        if handle_relayed_frame(state, &mut ctx, frame).await == Flow::Close {
            break;
        }
        if joined {
            let resync = modron_protocol::Frame::request(
                0,
                modron_protocol::Payload::Attach {
                    session_id: session,
                    last_seq: 0,
                },
            );
            if handle_frame(state, &mut ctx, true, resync).await == Flow::Close {
                break;
            }
        }
    }
    drop(ctx);
    let _ = writer.await;
    if let Some(registration) = registration {
        registration.finish().await;
    }
    Dial::Ended {
        up_for: up_since.elapsed(),
    }
}

/// The share key minted for a session by /remote-control, if any.
pub fn share_key_for(state: &ServerState, session: &SessionId) -> Option<String> {
    state.rc_keys.lock().unwrap().get(session).cloned()
}

/// Environment variable holding the bridge's `PUBLISH_TOKEN` secret.
pub const PUBLISH_TOKEN_ENV: &str = "MODRON_PUBLISH_TOKEN";

/// Read the publish token through `lookup` (the process environment in
/// production, a closure in tests). Empty counts as unset.
fn publish_token_from(lookup: impl Fn(&str) -> Option<String>) -> Result<String> {
    lookup(PUBLISH_TOKEN_ENV)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| {
            anyhow!("publishing needs MODRON_PUBLISH_TOKEN (the bridge's PUBLISH_TOKEN secret)")
        })
}

/// The `Authorization` header value the bridge expects for publishing.
pub fn publish_auth_header(token: &str) -> String {
    format!("Bearer {token}")
}

/// Publish an assistant answer as a shareable web page on the bridge.
/// The request is authorized by the deploy-time `PUBLISH_TOKEN` (read from
/// `MODRON_PUBLISH_TOKEN`), never by the share key. `key` only selects the
/// bridge's storage object. Pages are served at /p/<id> with no auth.
/// Fails before sending anything if the token is unset or empty.
pub async fn publish_page(
    bridge_base: &str,
    key: &str,
    session: SessionId,
    title: &str,
    content_md: &str,
) -> Result<String> {
    publish_page_with_lookup(
        |name| std::env::var(name).ok(),
        bridge_base,
        key,
        session,
        title,
        content_md,
    )
    .await
}

/// `publish_page` with the environment lookup injected (tests use this so
/// they never mutate process env).
async fn publish_page_with_lookup(
    lookup: impl Fn(&str) -> Option<String>,
    bridge_base: &str,
    key: &str,
    session: SessionId,
    title: &str,
    content_md: &str,
) -> Result<String> {
    let token = publish_token_from(lookup)?;
    check_bridge_transport(bridge_base)?;
    let base_url = bridge_http_base(bridge_base)?;
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "title": title,
        "session": session.to_string(),
        "content_md": content_md,
    });
    let response = client
        .post(format!("{base_url}/pages/{key}"))
        .header("content-type", "application/json")
        .header("authorization", publish_auth_header(&token))
        .body(body.to_string())
        .send()
        .await
        .context("posting the page to the bridge")?;
    if !response.status().is_success() {
        return Err(anyhow!(
            "bridge refused the page: {} {}",
            response.status(),
            response.text().await.unwrap_or_default()
        ));
    }
    let value: serde_json::Value = response.json().await.context("bridge page reply")?;
    let id = value
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("bridge page reply missing id"))?;
    Ok(format!("{base_url}/p/{id}"))
}

/// Check a bridge URL for the tunnel: `ws://` or `wss://` only (the same
/// rule the `remote_control` frame applies), and plaintext only to loopback
/// (see [`check_bridge_transport`]).
pub fn check_tunnel_bridge(bridge_base: &str) -> Result<()> {
    if !bridge_base.starts_with("ws://") && !bridge_base.starts_with("wss://") {
        return Err(anyhow!(
            "bridge url must start with ws:// or wss:// (deploy cloud/bridge first)"
        ));
    }
    // The share key and the whole transcript travel over this socket: plaintext
    // only to loopback.
    check_bridge_transport(bridge_base)
}

/// Every message of an error and its sources, joined with `: `.
fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// `text` with each non-empty secret replaced by `<redacted>`.
fn redact(text: &str, secrets: &[&str]) -> String {
    secrets
        .iter()
        .filter(|secret| !secret.is_empty())
        .fold(text.to_string(), |acc, secret| {
            acc.replace(secret, "<redacted>")
        })
}

/// Environment variable naming a bridge to open remote control against at
/// `modron serve` start (the `--remote-control` flag wins over it).
pub const REMOTE_CONTROL_ENV: &str = "MODRON_REMOTE_CONTROL";

/// The bridge URL `modron serve` opens at start: the flag when given,
/// else the environment variable. Empty counts as unset (for both).
pub fn resolve_remote_control(flag: Option<&str>, env: Option<&str>) -> Option<String> {
    [flag, env]
        .into_iter()
        .flatten()
        .find(|value| !value.is_empty())
        .map(str::to_string)
}

/// The project marker `serve --remote-control` needs before it can open a
/// tunnel: the `.modron` directory in the working directory (the same rule
/// the `remote_control` frame applies). An existing marker is kept. Without
/// one, a git repository (`.git` present, as a directory or a worktree
/// file) gets the marker created; anything else is refused, because remote
/// control hands the agent its working directory. Returns true when this
/// call created the marker.
pub fn prepare_project_marker(working_dir: &Path) -> Result<bool> {
    let marker = working_dir.join(".modron");
    if marker.exists() {
        return Ok(false);
    }
    if !working_dir.join(".git").exists() {
        return Err(anyhow!(
            "serve --remote-control needs a project in its working directory: run it inside a \
             git repository, or create an empty .modron directory there yourself"
        ));
    }
    std::fs::create_dir_all(&marker)
        .with_context(|| format!("creating the project marker {}", marker.display()))?;
    Ok(true)
}

/// The session `serve --remote-control` opens its tunnel for: the most
/// recently updated session that is not a worktree task's (task sessions
/// run inside their worktree, not the server's). `None` when there is none.
/// The store records no working directory, so "for this directory" means
/// "in this server's store", which is one repository per serve.
pub fn choose_serve_session(
    sessions: &[modron_core::SessionMeta],
    is_task: impl Fn(&SessionId) -> bool,
) -> Option<SessionId> {
    sessions
        .iter()
        .filter(|meta| !is_task(&meta.id))
        .max_by_key(|meta| meta.updated_at)
        .map(|meta| meta.id)
}

/// Write `contents` plus a newline to `path`, readable by the owner only.
fn write_private_file(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("opening the share url file {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // `mode` applies only on creation: tighten a file that already existed.
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restricting the share url file {}", path.display()))?;
    }
    file.write_all(format!("{contents}\n").as_bytes())
        .with_context(|| format!("writing the share url file {}", path.display()))
}

/// `modron serve --remote-control`: at start, open remote control for one
/// session without any client. The session is the most recent non-task
/// session in the store, or a new one when there is none. The tunnel
/// starts through [`start_remote_control`], the same path the
/// `remote_control` frame takes. Logs the share URL at info level and, when
/// `share_file` is given, writes it there (0600). Call after the listeners
/// are bound and [`prepare_project_marker`] has passed.
pub async fn start_serve_remote_control(
    state: &Arc<ServerState>,
    bridge_base: &str,
    share_file: Option<&Path>,
) -> Result<(String, SessionId)> {
    let sessions = state
        .store
        .list()
        .await
        .context("listing sessions for remote control")?;
    let session = match choose_serve_session(&sessions, |id| state.tasks.get(id).is_some()) {
        Some(session) => session,
        None => {
            state
                .store
                .create_session("remote session")
                .await
                .context("creating the remote-control session")?
                .id
        }
    };
    // Same live-session setup a Submit to a known session does.
    crate::dispatch::ensure_session_for(state, session);
    let url = start_remote_control(state, session, &state.working_dir, bridge_base)?;
    tracing::info!(%session, share_url = %url, "remote control open (serve --remote-control)");
    if let Some(path) = share_file {
        write_private_file(path, &url)?;
    }
    Ok((url, session))
}

/// Register a remote-control tunnel for `session`: validates the session,
/// mints the share key, spawns the tunnel task. Returns the open URL.
pub fn start_remote_control(
    state: &Arc<ServerState>,
    session: SessionId,
    working_dir: &Path,
    bridge_base: &str,
) -> Result<String> {
    check_tunnel_bridge(bridge_base)?;
    if !working_dir.join(".modron").exists() {
        return Err(anyhow!(
            "remote-control hands the agent its working directory - run from inside a \
             project (git repo) rather than an empty dir"
        ));
    }
    state
        .hub
        .handle(&session)
        .ok_or_else(|| anyhow!("no live session {session} - start a task or chat first"))?;
    let key = share_key();
    let (tunnel_url, open_url) = tunnel_urls(bridge_base, &key);
    state.rc_keys.lock().unwrap().insert(session, key.clone());
    let state_moved = Arc::clone(state);
    let bridge = bridge_base.to_string();
    let tunnel_task = tokio::spawn(async move {
        if let Err(e) = run_tunnel(state_moved, session, bridge, key).await {
            // `{:#}` keeps the whole chain (the transport cause), not just
            // the outermost context.
            // Only a refused token or a bad request ends a tunnel by itself
            // (see run_tunnel_timed); log that once, at error.
            tracing::error!(%session, error = format!("{e:#}"), "remote-control tunnel ended");
        }
    });
    // Abort the previous tunnel for this session, if any: a re-registration
    // used to stack duplicate pumpers on stale bridge sockets forever
    // (cycle-2 audit L7).
    if let Some(old) = state
        .rc_tunnels
        .lock()
        .unwrap()
        .insert(session, tunnel_task.abort_handle())
    {
        old.abort();
    }
    let _ = tunnel_url;
    Ok(open_url)
}

#[cfg(test)]
mod run_tests {
    use super::{check_run_id, entry_body};

    #[test]
    fn registration_body_carries_run_id_only_for_a_run() {
        let with = entry_body("t", "lbl", "cloud", "abcd", Some("run-1"), true);
        assert_eq!(with["run_id"], "run-1");
        assert_eq!(with["state"], "running");
        let without = entry_body("t", "lbl", "local", "abcd", None, false);
        assert!(without.get("run_id").is_none());
        assert_eq!(without["state"], "idle");
        assert_eq!(without["viewer_key"], "abcd");
    }

    #[test]
    fn run_ids_follow_the_bridge_rule() {
        assert!(check_run_id("3f2a-run-1").is_ok());
        assert!(check_run_id("").is_err());
        assert!(check_run_id("Has Upper").is_err());
        assert!(check_run_id("a/b").is_err());
        assert!(check_run_id(&"a".repeat(65)).is_err());
    }
}

#[cfg(test)]
mod publish_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// Serve one HTTP request on a local port. Returns the base URL and a
    /// receiver that yields the request head (lowercased) once it arrives.
    fn one_shot_bridge() -> (String, std::sync::mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            let head_end = loop {
                let n = sock.read(&mut chunk).unwrap();
                if n == 0 {
                    break buf.len();
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos + 4;
                }
            };
            let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
            let length: usize = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().ok())
                        .flatten()
                })
                .unwrap_or(0);
            while buf.len() < head_end + length {
                let n = sock.read(&mut chunk).unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let body = r#"{"id":"page-1"}"#;
            let reply = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(reply.as_bytes()).unwrap();
            let _ = tx.send(head.to_lowercase());
        });
        (format!("http://{addr}"), rx)
    }

    #[test]
    fn auth_header_is_bearer_token() {
        assert_eq!(publish_auth_header("s3cret"), "Bearer s3cret");
    }

    #[test]
    fn unset_or_empty_token_is_an_error() {
        assert_eq!(
            publish_token_from(|_| Some("t".into())).unwrap(),
            "t".to_string()
        );
        assert!(publish_token_from(|_| Some(String::new())).is_err());
        let message = publish_token_from(|_| None).unwrap_err().to_string();
        assert_eq!(
            message,
            "publishing needs MODRON_PUBLISH_TOKEN (the bridge's PUBLISH_TOKEN secret)"
        );
    }

    #[tokio::test]
    async fn publish_without_token_makes_no_request() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let err = publish_page_with_lookup(
            |_| None,
            &base,
            "0123456789abcdef0123456789abcdef",
            uuid::Uuid::new_v4(),
            "title",
            "body",
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("MODRON_PUBLISH_TOKEN"));
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "no connection may reach the bridge without a token"
        );
    }

    #[tokio::test]
    async fn publish_header_carries_publish_token_not_share_key() {
        let (base, head_rx) = one_shot_bridge();
        let key = "0123456789abcdef0123456789abcdef";
        let url = publish_page_with_lookup(
            |name| (name == PUBLISH_TOKEN_ENV).then(|| "pub-token-xyz".to_string()),
            &base,
            key,
            uuid::Uuid::new_v4(),
            "title",
            "# body",
        )
        .await
        .unwrap();
        assert!(url.ends_with("/p/page-1"), "unexpected url {url}");
        let head = head_rx.recv().unwrap();
        assert!(head.starts_with(&format!("post /pages/{key} ")), "{head}");
        assert!(
            head.contains("authorization: bearer pub-token-xyz"),
            "{head}"
        );
        assert!(
            !head.contains(&format!("bearer {key}")),
            "share key used as bearer: {head}"
        );
    }
}

#[cfg(test)]
mod key_tests {
    use super::{tunnel_urls, viewer_key};

    /// The same vector is produced by `viewerKeyOf` in the bridge
    /// (sha256("modron-viewer:" + key), first 16 bytes, hex). If this
    /// changes, share links stop meeting their agent on the relay.
    #[test]
    fn viewer_key_matches_the_bridge_derivation() {
        assert_eq!(
            viewer_key("0123456789abcdef0123456789abcdef"),
            "1e14cd6450e5f6e627a9c619807f506c"
        );
    }

    #[test]
    fn bridge_transport_allows_tls_anywhere_and_plaintext_only_on_loopback() {
        use super::check_bridge_transport;
        for ok in [
            "ws://localhost:8787",
            "ws://LOCALHOST",
            "ws://127.0.0.1",
            "ws://127.9.9.9:8787/base",
            "ws://[::1]:1",
            "http://127.0.0.1:9000",
            "wss://example.com",
            "https://example.com/base",
        ] {
            assert!(
                check_bridge_transport(ok).is_ok(),
                "{ok} should be accepted"
            );
        }
        for bad in [
            "ws://example.com",
            "ws://127.0.0.1.example.com",
            "ws://localhost@example.com",
            "ws://10.0.0.5:7000",
            "ws://[2001:db8::1]:8787",
            "http://x",
            "ftp://example.com",
        ] {
            assert!(
                check_bridge_transport(bad).is_err(),
                "{bad} should be refused"
            );
        }
        let err = check_bridge_transport("ws://example.com").unwrap_err();
        assert_eq!(
            err.to_string(),
            "refusing a plaintext ws:// bridge on a non-loopback host; use wss://"
        );
    }

    #[test]
    fn share_link_never_contains_the_agent_key() {
        let key = "0123456789abcdef0123456789abcdef";
        let (agent_url, share_url) = tunnel_urls("wss://bridge.example", key);
        assert_eq!(agent_url, format!("wss://bridge.example/agent/{key}"));
        assert!(!share_url.contains(key), "{share_url}");
        assert_eq!(
            share_url,
            format!("https://bridge.example/s/{}", viewer_key(key))
        );
        let (_, local) = tunnel_urls("ws://127.0.0.1:8787", key);
        assert_eq!(
            local,
            format!("http://127.0.0.1:8787/s/{}", viewer_key(key))
        );
    }
}

#[cfg(test)]
mod serve_start_tests {
    use super::*;
    use modron_core::SessionMeta;

    #[test]
    fn flag_wins_over_env_and_empty_counts_as_unset() {
        let wss = "wss://bridge.example";
        assert_eq!(
            resolve_remote_control(Some(wss), Some("wss://env.example")).as_deref(),
            Some(wss)
        );
        assert_eq!(
            resolve_remote_control(None, Some("wss://env.example")).as_deref(),
            Some("wss://env.example")
        );
        assert_eq!(
            resolve_remote_control(Some(""), Some("wss://env.example")).as_deref(),
            Some("wss://env.example")
        );
        assert_eq!(
            resolve_remote_control(Some(wss), Some("")).as_deref(),
            Some(wss)
        );
        assert_eq!(resolve_remote_control(None, None), None);
        assert_eq!(resolve_remote_control(Some(""), Some("")), None);
    }

    #[test]
    fn serve_bridge_follows_the_frame_rule() {
        assert!(check_tunnel_bridge("wss://bridge.example").is_ok());
        assert!(check_tunnel_bridge("ws://127.0.0.1:9000").is_ok());
        assert!(check_tunnel_bridge("ws://bridge.example").is_err());
        // The frame refuses http(s) for the tunnel even though publish takes it.
        assert!(check_tunnel_bridge("https://bridge.example").is_err());
        assert!(check_tunnel_bridge("bridge.example").is_err());
        assert!(check_tunnel_bridge("").is_err());
    }

    #[test]
    fn redaction_removes_secrets_and_skips_empty_ones() {
        let text = "failed for agent/SECRETKEY with token TOKEN";
        assert_eq!(
            redact(text, &["SECRETKEY", "TOKEN"]),
            "failed for agent/<redacted> with token <redacted>"
        );
        assert_eq!(redact(text, &[""]), text);
    }

    #[test]
    fn error_chain_keeps_every_cause() {
        #[derive(Debug)]
        struct Cause;
        impl std::fmt::Display for Cause {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("Connection refused")
            }
        }
        impl std::error::Error for Cause {}
        #[derive(Debug)]
        struct Outer(Cause);
        impl std::fmt::Display for Outer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("IO error")
            }
        }
        impl std::error::Error for Outer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }
        assert_eq!(error_chain(&Outer(Cause)), "IO error: Connection refused");
    }

    #[test]
    fn project_marker_is_kept_created_for_git_or_refused() {
        let dir = tempfile::tempdir().unwrap();
        // Neither a marker nor a git repository: refused, nothing created.
        assert!(prepare_project_marker(dir.path()).is_err());
        assert!(!dir.path().join(".modron").exists());
        // A git repository (a `.git` entry) gets the marker, and says so.
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        assert!(prepare_project_marker(dir.path()).unwrap());
        assert!(dir.path().join(".modron").is_dir());
        // An existing marker is kept as it is.
        assert!(!prepare_project_marker(dir.path()).unwrap());
        // A worktree-style `.git` file counts as a repository too.
        let worktree = tempfile::tempdir().unwrap();
        std::fs::write(worktree.path().join(".git"), "gitdir: /x").unwrap();
        assert!(prepare_project_marker(worktree.path()).unwrap());
    }

    /// A session row updated `hour` o'clock on 2026-11-16 (UTC).
    fn meta(id: u128, hour: u32) -> SessionMeta {
        let at = format!("2026-11-16T{hour:02}:00:00Z");
        serde_json::from_value(serde_json::json!({
            "id": SessionId::from_u128(id),
            "title": format!("s{id}"),
            "created_at": at,
            "updated_at": at,
            "message_count": 0,
        }))
        .expect("a session row")
    }

    #[test]
    fn serve_resumes_the_most_recent_non_task_session() {
        let sessions = vec![meta(1, 0), meta(2, 5), meta(3, 9)];
        let task = SessionId::from_u128(3);
        // Newest is a task session, so the next newest wins.
        assert_eq!(
            choose_serve_session(&sessions, |id| *id == task),
            Some(SessionId::from_u128(2))
        );
        assert_eq!(
            choose_serve_session(&sessions, |_| false),
            Some(SessionId::from_u128(3))
        );
        assert_eq!(choose_serve_session(&[], |_| false), None);
    }
}
