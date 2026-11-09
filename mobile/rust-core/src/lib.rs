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
//! Parity substrate (AGENTS.md UI/UX parity contract): all display
//! strings are produced here from the same modron-view primitives the TUI
//! and desktop use — [`modron_view::replay`] transcripts,
//! [`modron_view::ActivityTracker`] grouping, `ToolEvent::summary()`
//! rows, swarm rows, `human_bytes` sizes, the ask/yolo/deny mode words,
//! and the allow once / allow for this run / deny decision labels. The
//! snapshot pre-renders each row (`view::RenderedRow`) so a TSX screen
//! displays the shared copy verbatim instead of re-formatting it.
//!
//! The nsh shell that consumes this lives in `mobile/app` (Track 2 of the
//! mobile spike); the findings live in `mobile/SPIKE.md`.

pub mod client;
pub mod error;
pub mod view;

pub use client::{default_url, RemoteClient, CLIENT_NAME};
pub use error::ClientError;
pub use view::{
    approval_mode_label, connection_label, entry_lines, ConnectionState, DecisionOption,
    OAuthProgressView, PendingApprovalView, RenderedRow, SessionViewModel, ViewModelSnapshot,
    DECISION_LABELS,
};
