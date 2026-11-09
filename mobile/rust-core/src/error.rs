//! Client error surface: one error type for connect/handshake/wire/
//! request failures, serializable-friendly strings for the shell.

use std::fmt;

/// Everything that can go wrong on the remote-control client path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    /// TCP/WebSocket connect failed.
    Connect(String),
    /// The socket opened but the hello/welcome handshake failed.
    Handshake(String),
    /// The server spoke a different protocol version.
    ProtocolMismatch { server: u32, client: u32 },
    /// The connection is closed (or was never established).
    Closed,
    /// A request timed out waiting for its correlated answer.
    Timeout,
    /// The server answered with an `error` frame.
    Request(String),
    /// A frame failed to (de)serialize.
    Json(String),
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(e) => write!(f, "connect failed: {e}"),
            Self::Handshake(e) => write!(f, "handshake failed: {e}"),
            Self::ProtocolMismatch { server, client } => {
                write!(f, "protocol mismatch: server v{server}, client v{client}")
            }
            Self::Closed => write!(f, "connection closed"),
            Self::Timeout => write!(f, "request timed out"),
            Self::Request(e) => write!(f, "request failed: {e}"),
            Self::Json(e) => write!(f, "frame encoding: {e}"),
        }
    }
}

impl std::error::Error for ClientError {}
