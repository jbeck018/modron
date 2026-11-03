//! Auth-aware tier derivation: when the user configures no explicit
//! `[router] cheap_model/strong_model`, the tiers come from the models
//! their credentials can actually reach — a router that picks
//! unauthenticated anthropic tiers for a chatgpt+zai user is a bug, not
//! a default.

use crate::catalog::{catalog, Tier};

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
    const STRONG_PRICE_CEILING: f64 = 5.0;
    let reachable: Vec<_> = catalog()
        .iter()
        .enumerate()
        .filter(|(_, m)| authed_providers.contains(&m.provider))
        .collect();
    if reachable.is_empty() {
        return None;
    }

    let cheap = reachable
        .iter()
        .min_by(|(ia, a), (ib, b)| {
            a.input_per_m
                .partial_cmp(&b.input_per_m)
                .unwrap_or(std::cmp::Ordering::Equal)
                // Cheap-tier wins price ties (bool: true sorts AFTER
                // false, so compare reversed to favor it in a min); the
                // earliest catalog entry wins full ties.
                .then(b.tier.matches_cheap().cmp(&a.tier.matches_cheap()))
                .then(ia.cmp(ib))
        })
        .map(|(_, m)| m.name.clone())?;

    let strong_pool: Vec<_> = reachable
        .iter()
        .filter(|(_, m)| m.tier == Tier::Strong)
        .collect();
    // A reachable set with no Strong-tier catalog models (single cheap
    // provider) must not discard the whole derivation — the priciest
    // reachable model stands in for strong rather than letting the caller
    // fall back to unauthenticated defaults.
    let priciest_reachable = reachable.iter().max_by(|(ia, a), (ib, b)| {
        a.input_per_m
            .partial_cmp(&b.input_per_m)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(ib.cmp(ia))
    });
    let strong = strong_pool
        .iter()
        .filter(|(_, m)| m.input_per_m <= STRONG_PRICE_CEILING)
        .max_by(|(ia, a), (ib, b)| {
            a.input_per_m
                .partial_cmp(&b.input_per_m)
                .unwrap_or(std::cmp::Ordering::Equal)
                // Earlier catalog entry wins full ties in a max.
                .then(ib.cmp(ia))
        })
        .or_else(|| strong_pool.first())
        .or(priciest_reachable.as_ref())
        .map(|(_, m)| m.name.clone())?;

    Some((cheap, strong))
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
}
