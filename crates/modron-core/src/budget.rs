//! Budget governor — LEDGER item C2.
//!
//! Tracks cumulative usage for one task run and performs pre-flight
//! input-token checks per turn, tripping [`crate::Error::Budget`] with an
//! actionable message when a cap is hit. Provider-reported usage is ground
//! truth and is folded in via [`BudgetGovernor::feed_turn`] after every turn;
//! estimates ([`crate::estimate_tokens`]) are only used for pre-flight checks.

use crate::{Error, Result, Usage};

/// Tracks token spend for one task run against two caps:
///
/// - `max_input_tokens`: per-turn cap on the assembled context (pre-flight
///   estimate), enforced by [`BudgetGovernor::check_input`].
/// - `max_total_tokens`: cumulative cap on provider-reported usage for the
///   whole run, observed with [`BudgetGovernor::exceeded`].
#[derive(Clone, Debug)]
pub struct BudgetGovernor {
    max_total_tokens: u64,
    max_input_tokens: u64,
    cumulative: u64,
}

impl BudgetGovernor {
    /// Create a governor for one task run with the two caps.
    pub fn new(max_total_tokens: u64, max_input_tokens: u64) -> Self {
        Self {
            max_total_tokens,
            max_input_tokens,
            cumulative: 0,
        }
    }

    /// Fold one provider-reported turn usage into the cumulative total.
    /// Counts [`Usage::total`] (input + output as reported).
    pub fn feed_turn(&mut self, usage: Usage) {
        self.cumulative = self.cumulative.saturating_add(usage.total());
    }

    /// Pre-flight check: reject a turn whose assembled-context estimate
    /// exceeds the per-turn input cap. The error names both numbers so the
    /// operator can act on it.
    /// Check against an explicit cap (Phase 5i): the selected model's
    /// window, not the global config cap.
    pub fn check_input_with_cap(&self, estimated_input_tokens: u64, cap: u64) -> Result<()> {
        if estimated_input_tokens > cap {
            return Err(Error::Budget(format!(
                "turn context ~{estimated_input_tokens} est. tokens exceeds cap {cap} — \
                 compact or raise the cap"
            )));
        }
        Ok(())
    }

    /// Total provider-reported tokens accumulated so far.
    pub fn cumulative(&self) -> u64 {
        self.cumulative
    }

    /// Tokens left under the cumulative cap (saturates at 0).
    pub fn remaining(&self) -> u64 {
        self.max_total_tokens.saturating_sub(self.cumulative)
    }

    /// Whether cumulative reported usage has passed the total cap.
    pub fn exceeded(&self) -> bool {
        self.cumulative > self.max_total_tokens
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feed_turn_accumulates_and_remaining_shrinks() {
        let mut governor = BudgetGovernor::new(1000, 500);
        assert_eq!(governor.cumulative(), 0);
        assert_eq!(governor.remaining(), 1000);

        governor.feed_turn(Usage {
            input_tokens: 300,
            output_tokens: 100,
            cached_input_tokens: 0,
        });
        assert_eq!(governor.cumulative(), 400);
        assert_eq!(governor.remaining(), 600);

        governor.feed_turn(Usage {
            input_tokens: 700,
            output_tokens: 50,
            cached_input_tokens: 0,
        });
        assert_eq!(governor.cumulative(), 1150);
        assert_eq!(governor.remaining(), 0); // saturating, never underflows
        assert!(governor.exceeded());
    }

    #[test]
    fn check_input_trips_at_cap_with_actionable_error() {
        let governor = BudgetGovernor::new(10_000, 2048);
        assert!(governor.check_input_with_cap(2048, 2048).is_ok());

        let err = governor.check_input_with_cap(2049, 2048).unwrap_err();
        match err {
            Error::Budget(message) => {
                assert!(
                    message.contains("2049"),
                    "error should name the estimate: {message}"
                );
                assert!(
                    message.contains("2048"),
                    "error should name the cap: {message}"
                );
            }
            other => panic!("expected Error::Budget, got: {other}"),
        }
    }

    #[test]
    fn exceeded_trips_only_past_cap() {
        let mut governor = BudgetGovernor::new(100, 50);
        governor.feed_turn(Usage {
            input_tokens: 60,
            output_tokens: 40,
            cached_input_tokens: 0,
        });
        assert_eq!(governor.cumulative(), 100);
        assert!(!governor.exceeded(), "exactly at the cap is not over it");

        governor.feed_turn(Usage {
            input_tokens: 1,
            output_tokens: 0,
            cached_input_tokens: 0,
        });
        assert!(governor.exceeded());
    }
}
