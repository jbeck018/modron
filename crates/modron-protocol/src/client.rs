//! Client-side connection plumbing shared by every surface (desktop wire,
//! TUI remote, mobile wasm): hello-frame construction, the welcome
//! handshake check, and the request-id policy. The transports differ per
//! platform (tungstenite sync/async, web-sys callbacks); the PROTOCOL
//! pieces do not, and drifted copies of them are how surfaces desync.

use crate::{Frame, Payload, PROTOCOL_VERSION};
use std::sync::atomic::{AtomicU64, Ordering};

/// The hello frame: id 0 is reserved for it (the server never correlates
/// a request to id 0).
pub fn hello_frame(client: &str, token: Option<String>) -> Frame {
    Frame::request(
        0,
        Payload::Hello {
            client: client.to_string(),
            protocol: PROTOCOL_VERSION,
            token,
        },
    )
}

/// The handshake outcome: `Ok(server_version)` on a matching welcome.
pub type HandshakeResult = Result<String, String>;

/// Validate the first server frame. A matching `welcome` yields the
/// server's version string; a version mismatch, a refusal, a non-frame,
/// or silence yields the actionable message (the three clients shipped
/// three subtly different wordings for these - same rules, one copy now).
pub fn check_welcome(frame: Option<Frame>, target: &str) -> HandshakeResult {
    let Some(frame) = frame else {
        return Err(format!("server did not answer the handshake ({target})"));
    };
    match frame.payload {
        Payload::Welcome {
            server_version,
            protocol,
        } => {
            if protocol == PROTOCOL_VERSION {
                Ok(server_version)
            } else {
                Err(format!(
                    "protocol mismatch: server speaks {protocol}, this build speaks \
                     {PROTOCOL_VERSION} ({target})"
                ))
            }
        }
        Payload::Error { message, .. } => Err(format!("server refused the connection: {message}")),
        _ => Err(format!("server did not answer the handshake ({target})")),
    }
}

/// Monotonic request ids, never reused (the id the client assigns is how
/// replies correlate; restarting from 0 mid-connection could collide with
/// an in-flight reply).
#[derive(Default)]
pub struct RequestIds(AtomicU64);

impl RequestIds {
    pub fn new() -> Self {
        Self::default()
    }

    /// The next id (monotonic, never reused).
    pub fn next(&self) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_reserves_id_zero() {
        let frame = hello_frame("test-client", None);
        assert_eq!(frame.id, Some(0));
        match frame.payload {
            Payload::Hello {
                client, protocol, ..
            } => {
                assert_eq!(client, "test-client");
                assert_eq!(protocol, PROTOCOL_VERSION);
            }
            other => panic!("expected hello, got {other:?}"),
        }
    }

    #[test]
    fn ids_are_monotonic_and_never_reuse_zero() {
        let ids = RequestIds::new();
        assert_eq!(ids.next(), 0);
        assert_eq!(ids.next(), 1);
        assert_eq!(ids.next(), 2);
    }

    #[test]
    fn welcome_checks_agree_with_the_old_per_client_wording() {
        let welcome = Frame::event(
            None,
            0,
            Payload::Welcome {
                server_version: "9.9".into(),
                protocol: PROTOCOL_VERSION,
            },
        );
        assert_eq!(check_welcome(Some(welcome), "t").unwrap(), "9.9");

        let mismatch = Frame::event(
            None,
            0,
            Payload::Welcome {
                server_version: "9.9".into(),
                protocol: PROTOCOL_VERSION + 1,
            },
        );
        let err = check_welcome(Some(mismatch), "t").unwrap_err();
        assert!(err.contains("protocol mismatch"), "{err}");

        let refusal = Frame::event(
            None,
            0,
            Payload::Error {
                message: "bad token".into(),
                kind: None,
            },
        );
        let err = check_welcome(Some(refusal), "t").unwrap_err();
        assert!(err.contains("bad token"), "{err}");

        let err = check_welcome(None, "the-target").unwrap_err();
        assert!(err.contains("the-target"), "{err}");
    }
}
