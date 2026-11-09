//! modron-server — the remote-control session server (Phase 2).
//!
//! Owns live sessions behind a [`SessionHub`]: every engine sink writes
//! once into a per-session broadcast channel, N attached clients subscribe,
//! approvals route to the first client that answers, and `interrupt`
//! aborts the session's run task. This skeleton holds the crate slot only;
//! the Phase-2 tranche fills it in.

/// Crate slot placeholder — replaced by the Phase-2 implementation.
pub const PENDING: &str = "phase-2";

#[cfg(test)]
mod tests {
    #[test]
    fn skeleton_compiles() {
        assert_eq!(super::PENDING, "phase-2");
    }
}
