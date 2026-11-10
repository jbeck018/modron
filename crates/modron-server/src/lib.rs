//! modron-server — the remote-control session server (Phases 2+3).
//!
//! Owns live sessions behind a [`SessionHub`]: every engine sink writes
//! once into a per-session broadcast channel, N attached clients subscribe,
//! approvals route to the first client that answers, and `interrupt`
//! aborts the session's run task. The engine needs zero changes to reach
//! the wire — it already appends every event to its store, and that store
//! is a [`BroadcastingStore`] decorator that fans each persisted event out
//! to the hub.
//!
//! Transports share one frame vocabulary ([`modron_protocol::Frame`]):
//! one JSON document per WebSocket message remotely ([`ws`]) and NDJSON
//! lines on a Unix socket locally ([`unix`]). Loopback and Unix-socket
//! peers are trusted; remote peers authenticate with a device token
//! minted by `modron pair` ([`pairing`]).
//!
//! Layer map:
//! - [`hub`]: `SessionHub` / `SessionHandle` / `BroadcastingStore` —
//!   fan-out, seq ordering, approvals, interrupts, run slots;
//! - [`dispatch`]: the shared per-frame handler (hello/auth, attach
//!   snapshot ordering, submit/interrupt/approve/set-config/fork);
//! - [`ws`] / [`unix`]: transport framing;
//! - [`serve`]: `bind_server` (public, test-entry) + the `modron serve`
//!   foreground lifecycle;
//! - [`pairing`]: device tokens and the auth gate.

pub mod dispatch;
pub mod hub;
pub mod pairing;
pub mod serve;
pub mod unix;
pub mod ws;

pub use dispatch::{ConnCtx, Flow, ServerState};
pub use hub::{BroadcastingStore, RunFuture, SessionHandle, SessionHub};
pub use pairing::{
    check_token, default_clients_dir, generate_token, grant, list, revoke, DeviceToken,
};
pub use serve::{bind_server, run_serve, BoundServer, ServeOptions};
