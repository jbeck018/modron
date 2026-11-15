//! modron-mobile-core — the shared remote-client core for the mobile
//! shell (Phase 5, mobile track).
//!
//! A [`RemoteClient`] holds one WebSocket to the modron server, folds
//! every inbound [`modron_protocol::Frame`] into a serde-friendly
//! [`view::SessionViewModel`] behind a mutex, and exposes a polling API
//! for UI shells: [`RemoteClient::state`],
//! [`RemoteClient::take_new_entries`], and
//! [`RemoteClient::take_pending_approval`], plus one method per client
//! command (submit/interrupt/approve/set_config/list_sessions/...). No
//! callbacks and no async rendering hooks: a native shell (nsh TSX today,
//! Swift/Kotlin bridges later) ticks the snapshot on its own render loop.
//!
//! Connection lifecycle (2026-11-10, web-target parity): `connect` is
//! fire-and-forget — it validates the URL, flips the state to
//! `Connecting`, and completes the handshake + run loop on a background
//! task (tokio on native, `spawn_local` on wasm), so a UI thread never
//! blocks on the network (a browser main thread cannot). Callers watch
//! `state().connection` / `last_error`; the TSX shell polls the snapshot
//! every frame anyway.
//!
//! Platform split: the wire transport is per-target
//! (`transport_native`: tokio-tungstenite; `transport_web`: web-sys
//! `WebSocket` bridged through a channel, pings handled by the browser).
//! The fold, request correlation, and polling API are shared.
//!
//! Parity substrate (AGENTS.md UI/UX parity contract): all display
//! strings are produced here from the same modron-view primitives the TUI
//! and desktop use — [`modron_view::replay`] transcripts,
//! [`modron_view::ActivityTracker`] grouping, `ToolEvent::summary()`
//! rows, swarm rows, `human_bytes` sizes, the ask/yolo/deny mode words,
//! and the allow once / allow all / deny decision labels. The
//! snapshot pre-renders each row (`view::RenderedRow`) so a TSX screen
//! displays the shared copy verbatim instead of re-formatting it.
//!
//! The nsh shell that consumes this lives in `mobile/app` (Track 2 of the
//! mobile spike); the findings live in `mobile/SPIKE.md`.

pub mod attach;
pub mod client;
pub mod error;
pub mod view;

#[cfg(not(target_arch = "wasm32"))]
mod transport_native;
#[cfg(target_arch = "wasm32")]
mod transport_web;

#[cfg(not(target_arch = "wasm32"))]
pub(crate) use transport_native as transport;
#[cfg(target_arch = "wasm32")]
pub(crate) use transport_web as transport;

pub use attach::attachment_from_bytes;
#[cfg(not(target_arch = "wasm32"))]
pub use client::default_socket_path;
pub use client::{
    default_url, device_token_from_env, split_url_token, ConnectOptions, KeepaliveConfig,
    RemoteClient, CLIENT_NAME, DEVICE_TOKEN_ENV, UNDECODABLE_NOTE,
};
pub use error::ClientError;
pub use modron_protocol::{ApprovalModeWire, ConfigPatch, SessionMetaWire};
pub use modron_types::{Attachment, Effort, Phase, SandboxPolicy};
pub use modron_view::{QUEUE_LABEL, STOP_LABEL};
pub use view::{
    approval_mode_label, approval_mode_next, connection_label, context_label, effort_next,
    entry_lines, model_row_text, phase_label, phase_next, sandbox_next, session_row,
    ConnectionState, DecisionOption, ModelRowView, OAuthProgressView, PendingApprovalView,
    PendingPlanView, PlanDecisionOption, RenderedRow, SessionViewModel, ViewModelSnapshot,
    DECISION_LABELS, EFFORT_UNSET_LABEL, FILTER_LABEL, IDLE_LABEL, NEW_SESSION_LABEL,
    PLAN_DECISION_LABELS, RULES_NONE_LABEL, RUNNING_LABEL, SESSIONS_EMPTY_LABEL, SESSIONS_LABEL,
    SESSIONS_NO_MATCH_LABEL, SETTINGS_LABEL, SETTING_APPROVAL_LABEL, SETTING_COMPACT_LABEL,
    SETTING_CONTEXT_LABEL, SETTING_EFFORT_LABEL, SETTING_MODEL_LABEL, SETTING_PHASE_LABEL,
    SETTING_PROVIDER_LABEL, SETTING_RULES_LABEL, SETTING_SANDBOX_LABEL,
};
