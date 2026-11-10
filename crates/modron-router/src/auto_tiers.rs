//! Auth-aware tier derivation: when the user configures no explicit
//! `[router] cheap_model/strong_model`, the tiers come from the models
//! their credentials can actually reach — a router that picks
//! unauthenticated anthropic tiers for a chatgpt+zai user is a bug, not
//! a default.

use crate::catalog::{catalog, Tier};

/// Ordered tier candidates, the primary first. The engine's tier fallback
/// (see `EngineConfig::tier_fallbacks`) walks these when the primary's
/// provider fails with an auth-class error — a dead subscription token on
/// one provider must not take down a tier that another signed-in provider
/// can serve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TierCandidates {
    /// Model names, cheapest first.
    pub cheap: Vec<String>,
    /// Model names, strongest (per the derivation policy) first.
    pub strong: Vec<String>,
}

/// Derive (cheap, strong) tiers from the AUTHED providers (provider ids
/// as used by the catalog). None when no cataloged model is reachable.
///
/// Policy (documented, deterministic):
/// - cheap = the cheapest reachable model (min input $/M; ties prefer
///   Cheap-tier models, then catalog order). Subscription models at 0/0
///   win — their marginal cost is the plan.
/// - strong = the most expensive reachable Strong-tier model with
///   input $/M <= `STRONG_PRICE_CEILING` (limited-availability and
///   specialist tiers priced above it are not auto-picked); when that
///   filter empties the set, any reachable Strong-tier model.
pub fn derive_tiers(authed_providers: &[String]) -> Option<(String, String)> {
    let candidates = derive_tier_candidates(authed_providers)?;
    Some((
        candidates.cheap.first()?.clone(),
        candidates.strong.first()?.clone(),
    ))
}

/// The full ordered candidate lists behind [`derive_tiers`]: index 0 of
/// each list is exactly the model `derive_tiers` picks; the rest are the
/// remaining reachable candidates in descending preference.
///
/// - cheap: every reachable model, cheapest first (same ordering as the
///   primary pick).
/// - strong: reachable Strong-tier models at or under the price ceiling,
///   priciest first; then the Strong-tier models above the ceiling in
///   catalog order; when no Strong-tier model is reachable at all, every
///   reachable model priciest first (the stand-in rule).
pub fn derive_tier_candidates(authed_providers: &[String]) -> Option<TierCandidates> {
    const STRONG_PRICE_CEILING: f64 = 5.0;
    let reachable: Vec<_> = catalog()
        .iter()
        .enumerate()
        .filter(|(_, m)| authed_providers.contains(&m.provider))
        .collect();
    if reachable.is_empty() {
        return None;
    }

    let mut cheap_sorted = reachable.clone();
    cheap_sorted.sort_by(|(ia, a), (ib, b)| {
        a.input_per_m
            .partial_cmp(&b.input_per_m)
            .unwrap_or(std::cmp::Ordering::Equal)
            // Cheap-tier wins price ties (bool: true sorts AFTER false, so
            // compare reversed to favor it); the earliest catalog entry
            // wins full ties.
            .then(b.tier.matches_cheap().cmp(&a.tier.matches_cheap()))
            .then(ia.cmp(ib))
    });
    let cheap: Vec<String> = cheap_sorted.iter().map(|(_, m)| m.name.clone()).collect();

    // Descending price, earlier catalog entry first on ties (matches the
    // primary's max_by tie rule).
    let by_price_desc = |(ia, a): &(usize, &crate::catalog::ModelInfo),
                         (ib, b): &(usize, &crate::catalog::ModelInfo)| {
        b.input_per_m
            .partial_cmp(&a.input_per_m)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(ia.cmp(ib))
    };

    let strong_pool: Vec<_> = reachable
        .iter()
        .filter(|(_, m)| m.tier == Tier::Strong)
        .cloned()
        .collect();
    let strong: Vec<String> = if strong_pool.is_empty() {
        // No Strong-tier catalog model reachable (single cheap provider):
        // the priciest reachable models stand in rather than letting the
        // caller fall back to unauthenticated defaults.
        let mut all = reachable.clone();
        all.sort_by(by_price_desc);
        all.iter().map(|(_, m)| m.name.clone()).collect()
    } else {
        let mut under: Vec<_> = strong_pool
            .iter()
            .filter(|(_, m)| m.input_per_m <= STRONG_PRICE_CEILING)
            .cloned()
            .collect();
        under.sort_by(by_price_desc);
        // Above-ceiling Strong-tier models follow in catalog order — the
        // primary's "any reachable Strong-tier model" fallback order.
        let over = strong_pool
            .iter()
            .filter(|(_, m)| m.input_per_m > STRONG_PRICE_CEILING);
        under
            .iter()
            .chain(over)
            .map(|(_, m)| m.name.clone())
            .collect()
    };

    Some(TierCandidates { cheap, strong })
}

impl Tier {
    fn matches_cheap(&self) -> bool {
        matches!(self, Tier::Cheap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn empty_or_mock_only_auth_yields_none() {
        assert!(derive_tiers(&[]).is_none());
        assert!(derive_tiers(&ids(&["mock"])).is_none());
        assert!(derive_tier_candidates(&[]).is_none());
    }

    #[test]
    fn chatgpt_plus_anthropic_picks_plan_cheap_and_mid_flagship() {
        // Subscription 0/0 models win cheap (Cheap-tier preferred on
        // ties); strong stays under the price ceiling (fable-5-1 at $10
        // is limited-availability, opus-5-5 at $4 is the pick).
        let (cheap, strong) = derive_tiers(&ids(&["chatgpt", "anthropic"])).unwrap();
        assert_eq!(cheap, "gpt-5.6-terra", "0/0 cheap-tier first");
        assert_eq!(strong, "claude-opus-5-5", "priciest strong under $5/M");
    }

    #[test]
    fn zai_only_derives_within_zai() {
        let (cheap, strong) = derive_tiers(&ids(&["zai"])).unwrap();
        for name in [cheap.clone(), strong.clone()] {
            let m = crate::lookup(&name).unwrap();
            assert_eq!(m.provider, "zai", "{name}");
        }
    }

    #[test]
    fn google_only_stays_within_google() {
        let (cheap, strong) = derive_tiers(&ids(&["google"])).unwrap();
        assert_eq!(cheap, "gemini-2.5-flash", "cheap tier");
        assert_eq!(strong, "gemini-2.5-pro", "strong tier");
    }

    #[test]
    fn candidates_lead_with_the_derived_primaries() {
        // The fallback lists must begin with exactly what derive_tiers
        // picks — the engine matches a failed model against index 0.
        for authed in [
            ids(&["chatgpt", "anthropic"]),
            ids(&["zai"]),
            ids(&["google"]),
            ids(&["chatgpt", "anthropic", "zai", "google"]),
        ] {
            let (cheap, strong) = derive_tiers(&authed).unwrap();
            let candidates = derive_tier_candidates(&authed).unwrap();
            assert_eq!(candidates.cheap[0], cheap, "{authed:?}");
            assert_eq!(candidates.strong[0], strong, "{authed:?}");
        }
    }

    #[test]
    fn strong_candidates_cross_providers_after_the_primary() {
        // The dead-token scenario: opus is the strong primary, and a
        // signed-in ChatGPT plan must offer strong alternates on a
        // DIFFERENT provider for the engine's auth fallback.
        let candidates = derive_tier_candidates(&ids(&["chatgpt", "anthropic"])).unwrap();
        assert_eq!(candidates.strong[0], "claude-opus-5-5");
        let alt_providers: Vec<String> = candidates.strong[1..]
            .iter()
            .filter_map(|name| crate::lookup(name).map(|m| m.provider.clone()))
            .collect();
        assert!(
            alt_providers.iter().any(|p| p == "chatgpt"),
            "a chatgpt strong alternate must follow the anthropic primary: {:?}",
            candidates.strong
        );
        // Every candidate is Strong-tier and reachable.
        for name in &candidates.strong {
            let m = crate::lookup(name).unwrap();
            assert_eq!(m.tier, Tier::Strong, "{name}");
            assert!(["chatgpt", "anthropic"].contains(&m.provider.as_str()));
        }
    }

    #[test]
    fn candidates_have_no_duplicates() {
        let candidates =
            derive_tier_candidates(&ids(&["chatgpt", "anthropic", "zai", "google"])).unwrap();
        for list in [&candidates.cheap, &candidates.strong] {
            let mut seen = std::collections::HashSet::new();
            for name in list {
                assert!(seen.insert(name), "duplicate candidate {name}");
            }
        }
    }
}
