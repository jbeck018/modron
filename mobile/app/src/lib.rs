//! The app's Rust code: what `App.tsx` imports from `modron-remote/native`.
//! TypeScript declarations are generated from these types into
//! `.nsh/types/modron-remote/native.d.ts`.
//!
//! This module is the bridge to `modron-mobile-core` (the shared remote
//! client core): one `RemoteClient`, the folded `ViewModelSnapshot`
//! handed to TSX as JSON so the shell renders the shared copy verbatim
//! (AGENTS.md parity contract - no formatting in TSX).
//!
//! Every export is synchronous and non-blocking: `connect` starts the
//! background handshake (tokio on native, spawn_local on web) and the
//! shell watches `snapshot_json` for the connection state; `attach` and
//! `list_sessions` fire wire requests whose answers fold into the
//! snapshot. The TSX screen already polls the snapshot every frame, so
//! results simply arrive.
//!
//! Exported fns stay thin: the `#[module]` macro rewrites their bodies,
//! so all real work happens in the plain helpers below.

use modron_mobile_core::{ClientError, RemoteClient};
use nsh_core::prelude::*;
use std::sync::Mutex;

/// The crate's UI set (`nsh.json` `"rust"`).
#[native_module("modron-remote/native")]
fn native(registry: &mut Registry) {
    registry.module::<Remote>();
}

static CLIENT: Mutex<Option<RemoteClient>> = Mutex::new(None);

fn err_text(e: ClientError) -> String {
    e.to_string()
}

fn not_connected() -> String {
    "not connected".into()
}

fn lock_client() -> Result<std::sync::MutexGuard<'static, Option<RemoteClient>>, String> {
    CLIENT
        .lock()
        .map_err(|_| "client mutex poisoned".to_string())
}

fn parse_session(id: &str) -> Result<uuid::Uuid, String> {
    uuid::Uuid::parse_str(id).map_err(|e| format!("bad session id: {e}"))
}

fn parse_decision(decision: &str) -> Result<modron_protocol::Decision, String> {
    match decision {
        "allow_once" => Ok(modron_protocol::Decision::AllowOnce),
        "allow_all" | "allow_for_this_run" => Ok(modron_protocol::Decision::AllowAll),
        "deny" => Ok(modron_protocol::Decision::Deny),
        other => Err(format!("unknown decision: {other}")),
    }
}

// ---------------------------------------------------------------------------
// Plain helpers (no macro rewriting)
// ---------------------------------------------------------------------------

fn do_connect(url: &str, token: Option<&str>) -> Result<(), String> {
    let client = RemoteClient::connect(url, token).map_err(err_text)?;
    let mut guard = lock_client()?;
    *guard = Some(client);
    Ok(())
}

fn do_attach(session_id: &str, last_seq: u64) -> Result<(), String> {
    let session = parse_session(session_id)?;
    let guard = lock_client()?;
    match guard.as_ref() {
        Some(client) => client.attach(session, last_seq).map_err(err_text),
        None => Err(not_connected()),
    }
}

fn do_reconnect(url: &str, session_id: &str, token: Option<&str>) -> Result<(), String> {
    let session = parse_session(session_id)?;
    let guard = lock_client()?;
    match guard.as_ref() {
        Some(client) => client.reconnect(url, session, token).map_err(err_text),
        None => Err(not_connected()),
    }
}

fn do_submit(session_id: Option<&str>, task: String) -> Result<(), String> {
    let session = match session_id {
        Some(id) => Some(parse_session(id)?),
        None => None,
    };
    let guard = lock_client()?;
    match guard.as_ref() {
        Some(client) => client.submit(session, task).map_err(err_text),
        None => Err(not_connected()),
    }
}

fn do_interrupt(session_id: &str) -> Result<(), String> {
    let session = parse_session(session_id)?;
    let guard = lock_client()?;
    match guard.as_ref() {
        Some(client) => client.interrupt(session).map_err(err_text),
        None => Err(not_connected()),
    }
}

fn do_approve(request_id: String, decision: &str) -> Result<(), String> {
    let decision = parse_decision(decision)?;
    let guard = lock_client()?;
    match guard.as_ref() {
        Some(client) => client.approve(request_id, decision).map_err(err_text),
        None => Err(not_connected()),
    }
}

fn do_select_model(model: String, provider: String) -> Result<(), String> {
    let guard = lock_client()?;
    match guard.as_ref() {
        Some(client) => client.select_model(model, provider).map_err(err_text),
        None => Err(not_connected()),
    }
}

fn do_new_session(title: Option<&str>) -> Result<(), String> {
    let guard = lock_client()?;
    match guard.as_ref() {
        Some(client) => client.new_session(title).map_err(err_text),
        None => Err(not_connected()),
    }
}

fn do_list_sessions_json() -> Result<String, String> {
    let guard = lock_client()?;
    let client = guard.as_ref().ok_or_else(not_connected)?;
    // Fire the request; the rows land in the snapshot's sessions (the
    // TSX poll picks them up). Return the current rows so the caller can
    // render immediately.
    client.list_sessions().map_err(err_text)?;
    serde_json::to_string(&client.state().sessions).map_err(|e| e.to_string())
}

fn do_add_automation(name: &str, prompt: &str, interval: u32) -> Result<(), String> {
    let guard = lock_client()?;
    match guard.as_ref() {
        Some(client) => client
            .add_automation(name, prompt, interval)
            .map_err(err_text),
        None => Err(not_connected()),
    }
}

fn do_remove_automation(name: &str) -> Result<(), String> {
    let guard = lock_client()?;
    match guard.as_ref() {
        Some(client) => client.remove_automation(name).map_err(err_text),
        None => Err(not_connected()),
    }
}

fn do_list_automations_json() -> Result<String, String> {
    let guard = lock_client()?;
    let client = guard.as_ref().ok_or_else(not_connected)?;
    // Fire the request; rows land in the snapshot (the poll picks them
    // up). Return the current rows so the caller renders immediately.
    client.list_automations().map_err(err_text)?;
    serde_json::to_string(&client.state().automations).map_err(|e| e.to_string())
}

fn do_publish_page(title: &str, content_md: &str) -> Result<(), String> {
    let guard = lock_client()?;
    let client = guard.as_ref().ok_or_else(not_connected)?;
    let Some(id) = client.state().session_id else {
        return Err("no attached session".into());
    };
    client.publish_page(id, title, content_md).map_err(err_text)
}

fn do_auth_providers_json() -> Result<String, String> {
    let guard = lock_client()?;
    let client = guard.as_ref().ok_or_else(not_connected)?;
    // Fire the request; rows land in the snapshot (the poll picks them
    // up). Return the current rows so the caller renders immediately.
    client.auth_providers().map_err(err_text)?;
    serde_json::to_string(&client.state().providers).map_err(|e| e.to_string())
}

// Auth flows run against the attached session (progress streams on it).
fn attached_session_id(client: &RemoteClient) -> Result<uuid::Uuid, String> {
    client
        .state()
        .session_id
        .ok_or_else(|| "no attached session".to_string())
}

fn do_auth_start(provider: &str) -> Result<(), String> {
    let guard = lock_client()?;
    let client = guard.as_ref().ok_or_else(not_connected)?;
    let id = attached_session_id(client)?;
    client.auth_start(id, provider).map_err(err_text)
}

fn do_auth_set_key(provider: &str, key: &str) -> Result<(), String> {
    let guard = lock_client()?;
    let client = guard.as_ref().ok_or_else(not_connected)?;
    let id = attached_session_id(client)?;
    client.auth_set_key(id, provider, key).map_err(err_text)
}

fn do_set_config_json(patch: &str) -> Result<(), String> {
    let patch: modron_protocol::ConfigPatch =
        serde_json::from_str(patch).map_err(|e| format!("bad patch: {e}"))?;
    let guard = lock_client()?;
    let client = guard.as_ref().ok_or_else(not_connected)?;
    client.set_config(patch).map_err(err_text)
}

fn do_fork_session() -> Result<(), String> {
    let guard = lock_client()?;
    let client = guard.as_ref().ok_or_else(not_connected)?;
    let id = attached_session_id(client)?;
    client.fork_session(id).map_err(err_text)
}

fn do_create_task(title: &str, base: Option<&str>) -> Result<(), String> {
    let guard = lock_client()?;
    let client = guard.as_ref().ok_or_else(not_connected)?;
    client.create_task(title, base).map_err(err_text)
}

fn do_task_status() -> Result<(), String> {
    let guard = lock_client()?;
    let client = guard.as_ref().ok_or_else(not_connected)?;
    let id = attached_session_id(client)?;
    client.task_status(id).map_err(err_text)
}

fn do_merge_task(message: Option<&str>) -> Result<(), String> {
    let guard = lock_client()?;
    let client = guard.as_ref().ok_or_else(not_connected)?;
    let id = attached_session_id(client)?;
    client.merge_task(id, message).map_err(err_text)
}

fn do_router_defaults_json() -> Result<String, String> {
    let guard = lock_client()?;
    let client = guard.as_ref().ok_or_else(not_connected)?;
    client.router_defaults().map_err(err_text)?;
    serde_json::to_string(&client.state().router_defaults).map_err(|e| e.to_string())
}

fn do_persist_router(
    cheap: &str,
    strong: &str,
    cheap_fallback: Option<&str>,
    strong_fallback: Option<&str>,
) -> Result<(), String> {
    let guard = lock_client()?;
    let client = guard.as_ref().ok_or_else(not_connected)?;
    client
        .persist_router(cheap, strong, cheap_fallback, strong_fallback)
        .map_err(err_text)
}

/// The shared slash-command table (`modron_view::slash`), as JSON: the
/// composer's autocomplete and /help render it verbatim, so a command
/// typed on any client means the same thing.
fn do_slash_commands_json() -> String {
    #[derive(serde::Serialize)]
    #[serde(rename_all = "snake_case")]
    struct Row {
        name: &'static str,
        usage: &'static str,
        about: &'static str,
        scope: modron_view::slash::Scope,
    }
    let rows: Vec<Row> = modron_view::slash::SLASH_COMMANDS
        .iter()
        .map(|c| Row {
            name: c.name,
            usage: c.usage,
            about: c.about,
            scope: c.scope,
        })
        .collect();
    serde_json::to_string(&rows).unwrap_or_else(|_| "[]".into())
}

fn do_snapshot_json() -> String {
    let guard = match lock_client() {
        Ok(guard) => guard,
        Err(_) => return "{}".into(),
    };
    let snapshot = match guard.as_ref() {
        Some(client) => client.state(),
        None => return "{}".into(),
    };
    serde_json::to_string(&snapshot).unwrap_or_else(|_| "{}".into())
}

fn do_take_new_entries_json() -> String {
    let guard = match lock_client() {
        Ok(guard) => guard,
        Err(_) => return "[]".into(),
    };
    let entries = match guard.as_ref() {
        Some(client) => client.take_new_entries(),
        None => return "[]".into(),
    };
    serde_json::to_string(&entries).unwrap_or_else(|_| "[]".into())
}

// ---------------------------------------------------------------------------
// Exported module (thin bodies only)
// ---------------------------------------------------------------------------

/// The remote-control bridge: connect/attach to a modron server and poll
/// the folded view state as JSON.
#[module]
pub struct Remote;

#[module]
impl Remote {
    /// Loopback URL of the default remote port.
    pub fn default_url() -> String {
        modron_mobile_core::default_url()
    }

    /// Connect and run the hello/welcome handshake.
    pub fn connect(url: String, token: Option<String>) -> Result<(), String> {
        do_connect(&url, token.as_deref())
    }

    /// Reconnect from the last seen seq and re-attach; the server replays
    /// the missed events.
    pub fn reconnect(url: String, session_id: String, token: Option<String>) -> Result<(), String> {
        do_reconnect(&url, &session_id, token.as_deref())
    }

    /// Subscribe to a session's events; waits for the snapshot fold.
    pub fn attach(session_id: String, last_seq: u64) -> Result<(), String> {
        do_attach(&session_id, last_seq)
    }

    /// Stop receiving events (the session keeps running).
    pub fn detach() -> Result<(), String> {
        let guard = lock_client()?;
        match guard.as_ref() {
            Some(client) => client.detach().map_err(err_text),
            None => Err(not_connected()),
        }
    }

    /// Close the connection.
    pub fn disconnect() -> Result<(), String> {
        let guard = lock_client()?;
        match guard.as_ref() {
            Some(client) => {
                // The send result is irrelevant once the channel drops.
                client.disconnect();
                Ok(())
            }
            None => Err(not_connected()),
        }
    }

    /// Submit a task; an omitted session id starts a new session.
    pub fn submit(session_id: Option<String>, task: String) -> Result<(), String> {
        do_submit(session_id.as_deref(), task)
    }

    /// Abort the active run (the stop control; TUI Esc parity).
    pub fn interrupt(session_id: String) -> Result<(), String> {
        do_interrupt(&session_id)
    }

    /// Answer a pending approval: "allow_once" | "allow_all" | "deny"
    /// (allow-all is sticky for the rest of the run).
    pub fn approve(request_id: String, decision: String) -> Result<(), String> {
        do_approve(request_id, &decision)
    }

    /// Model switch: pins the model AND follows the provider (TUI /model
    /// parity).
    pub fn select_model(model: String, provider: String) -> Result<(), String> {
        do_select_model(model, provider)
    }

    /// Create an empty session without submitting a task.
    pub fn new_session(title: Option<String>) -> Result<(), String> {
        do_new_session(title.as_deref())
    }

    /// Delete a session on the server; refresh the list afterwards with
    /// `list_sessions_json` (the server notes the outcome instead of
    /// replaying the list).
    pub fn delete_session(session_id: String) -> Result<(), String> {
        let id = parse_session(&session_id)?;
        let guard = lock_client()?;
        match guard.as_ref() {
            Some(client) => client.delete_session(id).map_err(err_text),
            None => Err(not_connected()),
        }
    }

    /// List sessions; the JSON rows also land in the snapshot.
    pub fn list_sessions_json() -> Result<String, String> {
        do_list_sessions_json()
    }

    /// Provider auth rows (label, status, oauth capability); the rows
    /// also land in the snapshot.
    pub fn auth_providers_json() -> Result<String, String> {
        do_auth_providers_json()
    }

    /// Start a provider's server-side sign-in flow; progress streams
    /// into the snapshot's `oauth` field until the note arrives.
    pub fn auth_start(provider: String) -> Result<(), String> {
        do_auth_start(&provider)
    }

    /// Save an API key for a provider (the non-oauth path).
    pub fn auth_set_key(provider: String, key: String) -> Result<(), String> {
        do_auth_set_key(&provider, &key)
    }

    /// Apply a config patch (model, provider, sandbox, approval mode,
    /// allow/deny tool lists, goal) as its JSON shape; the server
    /// applies it to future turns only.
    pub fn set_config_json(patch: String) -> Result<(), String> {
        do_set_config_json(&patch)
    }

    /// Fork the attached session; the note names the new id.
    pub fn fork_session() -> Result<(), String> {
        do_fork_session()
    }

    /// Create a task session in its own worktree/branch; the reply
    /// adopts the new session (submits land there).
    pub fn create_task(title: String, base: Option<String>) -> Result<(), String> {
        do_create_task(&title, base.as_deref())
    }

    /// The attached session's task status (lands as a note).
    pub fn task_status() -> Result<(), String> {
        do_task_status()
    }

    /// Merge the attached task's branch into its base (lands as a note).
    pub fn merge_task(message: Option<String>) -> Result<(), String> {
        do_merge_task(message.as_deref())
    }

    /// The router's default-model picture; also lands in the snapshot.
    pub fn router_defaults_json() -> Result<String, String> {
        do_router_defaults_json()
    }

    /// Persist cheap/strong router defaults (fallbacks optional).
    pub fn persist_router(
        cheap: String,
        strong: String,
        cheap_fallback: Option<String>,
        strong_fallback: Option<String>,
    ) -> Result<(), String> {
        do_persist_router(
            &cheap,
            &strong,
            cheap_fallback.as_deref(),
            strong_fallback.as_deref(),
        )
    }

    /// The folded view state plus every pre-rendered string, as JSON
    /// (`{}` before the first connect).
    pub fn snapshot_json() -> String {
        do_snapshot_json()
    }

    /// Transcript rows committed since the last call, as a JSON array.
    pub fn take_new_entries_json() -> String {
        do_take_new_entries_json()
    }

    /// Last seen server seq for the attached session.
    pub fn last_seq() -> u64 {
        match lock_client() {
            Ok(guard) => guard.as_ref().map(|c| c.last_seq()).unwrap_or(0),
            Err(_) => 0,
        }
    }

    /// A scripted snapshot for the demo mode: renders the full screen
    /// (status, transcript, tool strip, approval sheet) with no server.
    pub fn demo_snapshot_json() -> String {
        demo_snapshot()
    }

    /// Add an automation (settings view); the refreshed rows land in the
    /// snapshot.
    pub fn add_automation(name: String, prompt: String, interval_minutes: u32) -> Result<(), String> {
        do_add_automation(&name, &prompt, interval_minutes)
    }

    /// Remove an automation by name; the refreshed rows land in the
    /// snapshot.
    pub fn remove_automation(name: String) -> Result<(), String> {
        do_remove_automation(&name)
    }

    /// Fire an automations list request; the snapshot's automations
    /// update when the answer lands. Returns the current rows.
    pub fn list_automations_json() -> Result<String, String> {
        do_list_automations_json()
    }

    /// Publish the attached session's transcript as a read-only page;
    /// the URL lands in the snapshot's last_note.
    pub fn publish_page(title: String, content_md: String) -> Result<(), String> {
        do_publish_page(&title, &content_md)
    }

    /// The shared slash-command table as JSON (name/usage/about/scope):
    /// the composer's autocomplete and /help render it verbatim.
    pub fn slash_commands_json() -> String {
        do_slash_commands_json()
    }
}

// The demo fixture: shared types only - the screen renders exactly what a
// live session would hand it.
fn demo_snapshot() -> String {
    use modron_types::{ToolCall, Usage};
    use modron_mobile_core::{ConnectionState, PendingApprovalView, ViewModelSnapshot};
    use modron_protocol::{ApprovalModeWire, SessionMetaWire};

    let snapshot = ViewModelSnapshot {
        connection: ConnectionState::Connected,
        connection_label: "connected".into(),
        session_id: Some(uuid::Uuid::nil()),
        server_version: "0.1.2".into(),
        running: true,
        running_label: "running".into(),
        status_line: "claude-opus-5 \u{b7} 18.4k tok \u{b7} cache 71% \u{b7} ask".into(),
        streaming_text: "Migrating the router now; ".into(),
        streaming_reasoning: String::new(),
        tool_lines: vec!["\u{25cf} bash \u{2026}".into()],
        turn_summary_line: Some(
            "\u{23fa} 4 tools \u{b7} read \u{d7}2 \u{b7} glob \u{b7} bash".into(),
        ),
        subagent_rows: Vec::new(),
        pending_approval: Some(PendingApprovalView::new(
            "demo-req-1".into(),
            &ToolCall {
                id: "c1".into(),
                name: "edit".into(),
                arguments: serde_json::json!({"path": "crates/modron-router/src/lib.rs"}),
            },
        )),
        approval_mode: Some(ApprovalModeWire::Ask),
        approval_mode_label: "ask".into(),
        cumulative_usage: Usage {
            input_tokens: 13_000,
            output_tokens: 5_400,
            cached_input_tokens: 47_000,
        },
        cumulative_tokens: 18_400,
        session_cache_pct: Some(71.0),
        turns_completed: 3,
        tool_calls_total: 11,
        model: "claude-opus-5".into(),
        provider: "anthropic".into(),
        goal: String::new(),
        sandbox: "off".into(),
        allow_tools: Vec::new(),
        deny_tools: Vec::new(),
        automations: vec![modron_protocol::AutomationWire {
            name: "nightly repo audit".into(),
            prompt: "audit the repo for rot and open notes".into(),
            interval_minutes: 1440,
            last_run: None,
        }],
        providers: vec![
            modron_protocol::AuthProviderWire {
                id: "anthropic".into(),
                label: "Anthropic (Claude)".into(),
                status: "subscription (oauth)".into(),
                oauth_capable: true,
            },
            modron_protocol::AuthProviderWire {
                id: "openai-compat".into(),
                label: "OpenAI / ChatGPT".into(),
                status: "api key".into(),
                oauth_capable: true,
            },
            modron_protocol::AuthProviderWire {
                id: "zai".into(),
                label: "Z.ai (GLM)".into(),
                status: "not configured".into(),
                oauth_capable: true,
            },
            modron_protocol::AuthProviderWire {
                id: "openrouter".into(),
                label: "OpenRouter".into(),
                status: "not configured".into(),
                oauth_capable: false,
            },
        ],
        sessions: vec![
            SessionMetaWire {
                id: uuid::Uuid::nil(),
                title: "migrate the router to the new hub".into(),
                created_at: "2026-11-09T09:00:00Z".into(),
                updated_at: "2026-11-09T12:30:00Z".into(),
                message_count: 8,
                task_branch: None,
                task_state: None,
                working_dir: None,
            },
            SessionMetaWire {
                id: uuid::Uuid::from_u128(1),
                title: "audit the tools crate".into(),
                created_at: "2026-11-08T15:00:00Z".into(),
                updated_at: "2026-11-08T17:10:00Z".into(),
                message_count: 5,
                task_branch: None,
                task_state: None,
                working_dir: None,
            },
        ],
        last_seq: 42,
        last_error: None,
        last_note: Some("switched to claude-opus-5 (takes effect on the next turn)".into()),
        oauth: None,
        router_defaults: Some(modron_protocol::RouterDefaults {
            authed: vec!["anthropic".into(), "openai-compat".into()],
            recommended_cheap: Some("gpt-5.6-terra".into()),
            recommended_strong: Some("claude-opus-5-5".into()),
            cheap_fallback: vec!["glm-5.3".into()],
            strong_fallback: vec!["gpt-6-astra".into()],
        }),
        history: vec![
            row(modron_view::TranscriptEntry::Task {
                text: "migrate the router to the new hub".into(),
            }),
            row(modron_view::TranscriptEntry::ToolSummary {
                line: "\u{23fa} 4 tools \u{b7} read \u{d7}2 \u{b7} glob \u{b7} bash".into(),
                events: Vec::new(),
            }),
            row(modron_view::TranscriptEntry::Assistant {
                text: "The hub owns fan-out now; moving the router's sinks over.".into(),
            }),
            row(modron_view::TranscriptEntry::Usage {
                model: "claude-opus-5".into(),
                usage: Usage {
                    input_tokens: 4_100,
                    output_tokens: 1_800,
                    cached_input_tokens: 16_000,
                },
            }),
        ],
    };
    serde_json::to_string(&snapshot).unwrap_or_else(|_| "{}".into())
}

/// One rendered transcript row: the typed entry plus its shared strings.
fn row(entry: modron_view::TranscriptEntry) -> modron_mobile_core::RenderedRow {
    let lines = modron_mobile_core::entry_lines(&entry);
    modron_mobile_core::RenderedRow { entry, lines }
}
