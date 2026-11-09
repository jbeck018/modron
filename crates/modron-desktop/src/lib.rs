//! modron-desktop -- the GPUI Kit remote client (remote-control Phase 4).
//!
//! Layering: [`app_state::AppState`] folds protocol frames into UI state
//! (plain Rust, unit-tested without gpui); [`wire`] speaks the WebSocket
//! protocol; [`demo`] is a built-in mock server for standalone runs; [`ui`]
//! is a thin GPUI Kit renderer over the state.

pub mod app_state;
pub mod demo;
pub mod ui;
pub mod wire;

pub use app_state::{AppState, ConnState, Row, SessionRow};
