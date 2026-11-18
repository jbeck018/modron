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

/// What a matching welcome carries: the server's version and the project
/// command names of its working directory (empty from older servers).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WelcomeInfo {
    pub server_version: String,
    pub commands: Vec<String>,
}

/// Validate the first server frame. A matching `welcome` yields the
/// server's version string; a version mismatch, a refusal, a non-frame,
/// or silence yields the actionable message (the three clients shipped
/// three subtly different wordings for these - same rules, one copy now).
pub fn check_welcome(frame: Option<Frame>, target: &str) -> HandshakeResult {
    check_welcome_info(frame, target).map(|info| info.server_version)
}

/// [`check_welcome`] that also returns the welcome's project commands.
pub fn check_welcome_info(frame: Option<Frame>, target: &str) -> Result<WelcomeInfo, String> {
    let Some(frame) = frame else {
        return Err(format!("server did not answer the handshake ({target})"));
    };
    match frame.payload {
        Payload::Welcome {
            server_version,
            protocol,
            commands,
        } => {
            if protocol == PROTOCOL_VERSION {
                Ok(WelcomeInfo {
                    server_version,
                    commands,
                })
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
pub struct RequestIds(AtomicU64);

impl Default for RequestIds {
    fn default() -> Self {
        Self::new()
    }
}

impl RequestIds {
    /// Ids start at 1: id 0 is reserved for the hello frame, so a reply to
    /// hello can never be mistaken for the reply to a later request.
    pub fn new() -> Self {
        Self(AtomicU64::new(1))
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
        assert_eq!(ids.next(), 1);
        assert_eq!(ids.next(), 2);
        assert_eq!(RequestIds::default().next(), 1, "default also skips 0");
    }

    #[test]
    fn welcome_carries_the_project_commands_and_an_old_one_has_none() {
        let with = Frame::event(
            None,
            0,
            Payload::Welcome {
                server_version: "1".into(),
                protocol: PROTOCOL_VERSION,
                commands: vec!["deploy".into(), "review-pr".into()],
            },
        );
        let info = check_welcome_info(Some(with), "t").unwrap();
        assert_eq!(info.commands, vec!["deploy", "review-pr"]);
        assert_eq!(info.server_version, "1");
        // An older server's welcome has no `commands` key at all: an empty
        // list is omitted on the wire, so that frame is what it sends.
        let old = Frame::event(
            None,
            0,
            Payload::Welcome {
                server_version: "0.1.2".into(),
                protocol: PROTOCOL_VERSION,
                commands: Vec::new(),
            },
        );
        let json = serde_json::to_string(&old).unwrap();
        assert!(!json.contains("commands"), "{json}");
        let frame: Frame = serde_json::from_str(&json).unwrap();
        assert!(check_welcome_info(Some(frame), "t")
            .unwrap()
            .commands
            .is_empty());
    }

    #[test]
    fn welcome_checks_agree_with_the_old_per_client_wording() {
        let welcome = Frame::event(
            None,
            0,
            Payload::Welcome {
                server_version: "9.9".into(),
                protocol: PROTOCOL_VERSION,
                commands: Vec::new(),
            },
        );
        assert_eq!(check_welcome(Some(welcome), "t").unwrap(), "9.9");

        let mismatch = Frame::event(
            None,
            0,
            Payload::Welcome {
                server_version: "9.9".into(),
                protocol: PROTOCOL_VERSION + 1,
                commands: Vec::new(),
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
