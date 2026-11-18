//! In-memory provider-key overlay (PLAN 10.9, item 2).
//!
//! A host running `modron serve --portal-credentials` fetches the owner's
//! provider keys from the bridge vault and keeps them here, in process memory
//! only. Providers consult the overlay AFTER the process environment (which
//! includes the env file once sourced), so a locally configured key always
//! wins. The overlay is never written to disk and never logged: the
//! `Debug` impl shows names only.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

/// The fetched key set, keyed by environment variable name.
#[derive(Default)]
pub struct CredentialOverlay {
    values: RwLock<BTreeMap<String, String>>,
}

impl CredentialOverlay {
    pub fn new() -> Self {
        Self::default()
    }

    /// The overlay's value for an environment variable name.
    pub fn get(&self, env_var: &str) -> Option<String> {
        self.values
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(env_var)
            .cloned()
    }

    /// Replaces the whole set. A name missing from `values` disappears from
    /// the overlay. Returns the names now held, sorted.
    pub fn replace(&self, values: BTreeMap<String, String>) -> Vec<String> {
        let names = values.keys().cloned().collect();
        *self.values.write().unwrap_or_else(|e| e.into_inner()) = values;
        names
    }

    /// The environment variable names currently held, sorted.
    pub fn names(&self) -> Vec<String> {
        self.values
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect()
    }
}

impl fmt::Debug for CredentialOverlay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialOverlay")
            .field("names", &self.names())
            .finish()
    }
}

static SHARED: OnceLock<Arc<CredentialOverlay>> = OnceLock::new();
static ENABLED: AtomicBool = AtomicBool::new(false);

/// The process-wide overlay the serve path fills and the provider builder reads.
pub fn shared() -> Arc<CredentialOverlay> {
    SHARED
        .get_or_init(|| Arc::new(CredentialOverlay::new()))
        .clone()
}

/// Turns the overlay on for this process (`modron serve --portal-credentials`).
/// Until then provider construction is exactly as before.
pub fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}

/// Whether the overlay is on for this process.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn replace_sets_and_removes_names() {
        let overlay = CredentialOverlay::new();
        assert_eq!(overlay.get("ZAI_API_KEY"), None);
        let names = overlay.replace(set(&[("ZAI_API_KEY", "z-1"), ("GROQ_API_KEY", "g-1")]));
        assert_eq!(names, vec!["GROQ_API_KEY", "ZAI_API_KEY"]);
        assert_eq!(overlay.get("ZAI_API_KEY").as_deref(), Some("z-1"));
        overlay.replace(set(&[("GROQ_API_KEY", "g-2")]));
        assert_eq!(
            overlay.get("ZAI_API_KEY"),
            None,
            "a removed name disappears"
        );
        assert_eq!(overlay.get("GROQ_API_KEY").as_deref(), Some("g-2"));
        overlay.replace(BTreeMap::new());
        assert!(overlay.names().is_empty());
    }

    #[test]
    fn debug_shows_names_never_values() {
        let overlay = CredentialOverlay::new();
        overlay.replace(set(&[("ZAI_API_KEY", "super-secret-value")]));
        let shown = format!("{overlay:?}");
        assert!(shown.contains("ZAI_API_KEY"));
        assert!(!shown.contains("super-secret-value"), "{shown}");
    }
}
