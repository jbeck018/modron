//! Ownership rule for sessions handed between hosts (PLAN.md 10.8, C6c).
//!
//! `modron handoff export` appends a [`SessionEvent::HandedOff`] as the last
//! line of the exporting host's log. While that event is the last event, this
//! host must not change the session: runs, compaction and `!` commands are
//! refused with [`run_refusal`]. An import of a longer log that continues
//! past the HandedOff line replaces the log and so clears the rule.

use modron_types::{SessionEvent, SessionId};

/// The carrier ref of the handoff that holds this session, when the session's
/// last event is a [`SessionEvent::HandedOff`]. `None` means this host owns it.
pub fn handed_off_ref(events: &[SessionEvent]) -> Option<&str> {
    match events.last() {
        Some(SessionEvent::HandedOff { carrier_ref, .. }) => Some(carrier_ref.as_str()),
        _ => None,
    }
}

/// The refusal text for any change to a session this host handed off. It names
/// the import command that takes the session back.
pub fn refusal(session_id: SessionId, carrier_ref: &str) -> String {
    format!(
        "session {session_id} was handed off to another host (ref {carrier_ref}); \
         run `modron handoff import {carrier_ref}` on this host to take it back"
    )
}

/// `Some(refusal text)` when the session must not change on this host.
pub fn run_refusal(session_id: SessionId, events: &[SessionEvent]) -> Option<String> {
    handed_off_ref(events).map(|carrier_ref| refusal(session_id, carrier_ref))
}

#[cfg(test)]
mod tests {
    use super::*;
    use modron_types::Timestamp;

    fn task(text: &str) -> SessionEvent {
        SessionEvent::Task { text: text.into() }
    }

    fn handed_off() -> SessionEvent {
        SessionEvent::HandedOff {
            to: None,
            at: Timestamp::now(),
            carrier_ref: "refs/heads/modron/handoff/x".into(),
        }
    }

    #[test]
    fn only_a_trailing_handed_off_blocks() {
        let id = SessionId::nil();
        assert!(run_refusal(id, &[task("a"), handed_off()]).is_some());
        // A longer log that continues past the HandedOff line clears it.
        assert!(run_refusal(id, &[task("a"), handed_off(), task("b")]).is_none());
        assert!(run_refusal(id, &[]).is_none());
    }

    #[test]
    fn refusal_names_the_import_command() {
        let msg = refusal(SessionId::nil(), "refs/heads/modron/handoff/x");
        assert!(msg.contains("modron handoff import refs/heads/modron/handoff/x"));
    }
}
