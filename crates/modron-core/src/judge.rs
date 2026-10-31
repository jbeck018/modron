//! Async decision-model support (Phase-3b M0/M1): the LLM-judge prompt and
//! verdict parser, plus the [`AsyncRouter`] seam's test double.
//!
//! Field-validated pattern (see LEDGER Phase 3b): a tiny local model (0.5B–
//! 1.5B via Ollama, or any cheap hosted model) acts as **router, not judge**
//! — it only classifies task complexity; output quality is never evaluated.
//! One routing call costs ~100-500ms and ~$0.0001.

use crate::ActionKind;

/// System prompt for the LLM judge. Terse by design: the judge call is
/// overhead on every consulted decision.
pub const ROUTE_JUDGE_SYSTEM: &str = "You classify coding tasks for model routing. Reply with \
     exactly one word: MECHANICAL (search, read, list, summarize, look up, explain — no creation, \
     no multi-step work) or JUDGMENT (implement, fix, refactor, plan, design, review, debug — \
     creation or multi-step reasoning).";

/// Parse a judge response. Tolerates casing, whitespace, and a leading
/// "verdict:" style preamble; anything unparseable is `None` (the caller
/// keeps the sync selector's choice).
pub fn parse_route_verdict(text: &str) -> Option<crate::RouteVerdict> {
    let upper = text.to_uppercase();
    // First decisive word wins — judge models sometimes ramble past the
    // verdict, and the cheapest models put it first.
    for token in upper.split(|c: char| !c.is_ascii_alphabetic()) {
        match token {
            "MECHANICAL" => return Some(crate::RouteVerdict::Mechanical),
            "JUDGMENT" => return Some(crate::RouteVerdict::Judgment),
            _ => {}
        }
    }
    None
}

/// Map a verdict to the synthetic action kind used to re-query the sync
/// selector (Search → cheap tier, Plan → strong tier — the selector's tier
/// config, overrides included, stays the single source of truth).
pub fn verdict_action(verdict: crate::RouteVerdict) -> ActionKind {
    match verdict {
        crate::RouteVerdict::Mechanical => ActionKind::Search,
        crate::RouteVerdict::Judgment => ActionKind::Plan,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RouteVerdict;

    #[test]
    fn parses_clean_verdicts() {
        assert_eq!(
            parse_route_verdict("MECHANICAL"),
            Some(RouteVerdict::Mechanical)
        );
        assert_eq!(
            parse_route_verdict("judgment"),
            Some(RouteVerdict::Judgment)
        );
        assert_eq!(
            parse_route_verdict("  Judgment\n"),
            Some(RouteVerdict::Judgment)
        );
    }

    #[test]
    fn parses_chatty_responses_first_word_wins() {
        assert_eq!(
            parse_route_verdict("verdict: MECHANICAL because it is a search"),
            Some(RouteVerdict::Mechanical)
        );
        assert_eq!(
            parse_route_verdict("JUDGMENT. This task requires implementing"),
            Some(RouteVerdict::Judgment)
        );
    }

    #[test]
    fn garbage_is_none_not_guess() {
        assert_eq!(parse_route_verdict(""), None);
        assert_eq!(parse_route_verdict("I cannot classify this"), None);
        assert_eq!(parse_route_verdict("maybe"), None);
    }

    #[test]
    fn verdict_maps_to_tier_actions() {
        assert_eq!(verdict_action(RouteVerdict::Mechanical), ActionKind::Search);
        assert_eq!(verdict_action(RouteVerdict::Judgment), ActionKind::Plan);
    }
}
