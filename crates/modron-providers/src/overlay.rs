//! Per-turn provider keys from the portal overlay (PLAN 10.9, item 2).
//!
//! `modron serve --portal-credentials` keeps the owner's vault keys in
//! `modron_core::credential_overlay`. An [`OverlayProvider`] wraps one
//! env-backed provider and resolves its key on every turn: the process
//! environment first (a local key wins), then the overlay. The inner
//! provider is rebuilt only when that key changes, so a key removed from the
//! vault stops working on the next turn, and nothing is cached on disk.

use crate::factory::provider_from_env_with;
use async_trait::async_trait;
use modron_core::credential_overlay::CredentialOverlay;
use modron_core::{ModelProvider, Result, StreamDelta, TurnRequest, TurnResponse};
use std::sync::{Arc, Mutex};

/// Reads one environment variable name (process env in production).
pub type EnvFn = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

struct Current {
    key: Option<String>,
    provider: Arc<dyn ModelProvider>,
}

/// A provider whose key comes from the environment, then the overlay.
pub struct OverlayProvider {
    provider_id: String,
    env_var: &'static str,
    overlay: Arc<CredentialOverlay>,
    process_env: EnvFn,
    current: Mutex<Option<Current>>,
}

impl OverlayProvider {
    /// Wraps the registry provider `provider_id`. `None` when the id has no
    /// env-backed key in the auth registry (for example `mock` or `chatgpt`),
    /// so the caller builds those as before.
    pub fn new(
        provider_id: &str,
        overlay: Arc<CredentialOverlay>,
        process_env: EnvFn,
    ) -> Option<Self> {
        let info = modron_core::auth::provider_registry()
            .into_iter()
            .find(|p| p.provider_id == provider_id)?;
        Some(Self {
            provider_id: provider_id.to_string(),
            env_var: info.env_var,
            overlay,
            process_env,
            current: Mutex::new(None),
        })
    }

    // The process environment first (empty counts as unset), then the overlay.
    fn lookup(&self, name: &str) -> Option<String> {
        (self.process_env)(name)
            .filter(|value| !value.is_empty())
            .or_else(|| self.overlay.get(name))
    }

    // The inner provider for the key in effect now. Rebuilt only when the key
    // changed; a build error (no key, bad key) is returned for this turn.
    fn current(&self) -> Result<Arc<dyn ModelProvider>> {
        let key = self.lookup(self.env_var);
        let mut slot = self.current.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = slot.as_ref() {
            if cached.key == key {
                return Ok(Arc::clone(&cached.provider));
            }
        }
        *slot = None;
        let provider = provider_from_env_with(&self.provider_id, |name| self.lookup(name))?;
        *slot = Some(Current {
            key,
            provider: Arc::clone(&provider),
        });
        Ok(provider)
    }
}

#[async_trait]
impl ModelProvider for OverlayProvider {
    fn id(&self) -> &str {
        &self.provider_id
    }

    async fn turn(&self, req: Arc<TurnRequest>) -> Result<TurnResponse> {
        let provider = self.current()?;
        provider.turn(req).await
    }

    async fn turn_streaming(
        &self,
        req: Arc<TurnRequest>,
        tx: tokio::sync::mpsc::UnboundedSender<StreamDelta>,
    ) -> Result<TurnResponse> {
        let provider = self.current()?;
        provider.turn_streaming(req, tx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use modron_core::Error;

    fn env_with(vars: &[(&str, &str)]) -> EnvFn {
        let map: std::collections::HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Arc::new(move |name| map.get(name).cloned())
    }

    fn overlay_with(pairs: &[(&str, &str)]) -> Arc<CredentialOverlay> {
        let overlay = Arc::new(CredentialOverlay::new());
        overlay.replace(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        );
        overlay
    }

    fn missing_key_var(err: &Error) -> Option<String> {
        match err {
            Error::Config(what, _) => Some(what.clone()),
            _ => None,
        }
    }

    #[test]
    fn only_env_backed_registry_ids_are_wrapped() {
        let env = env_with(&[]);
        let overlay = overlay_with(&[]);
        assert!(OverlayProvider::new("groq", overlay.clone(), env.clone()).is_some());
        assert!(OverlayProvider::new("mock", overlay.clone(), env.clone()).is_none());
        assert!(OverlayProvider::new("chatgpt", overlay, env).is_none());
    }

    #[test]
    fn overlay_key_is_used_when_no_local_key_exists() {
        let p = OverlayProvider::new(
            "groq",
            overlay_with(&[("GROQ_API_KEY", "gsk-from-vault")]),
            env_with(&[]),
        )
        .unwrap();
        assert!(p.current().is_ok(), "the overlay key builds the provider");
    }

    #[test]
    fn missing_key_is_a_config_error_naming_the_variable() {
        let p = OverlayProvider::new("groq", overlay_with(&[]), env_with(&[])).unwrap();
        let err = p.current().err().expect("no key, no provider");
        assert!(missing_key_var(&err).unwrap().contains("GROQ_API_KEY"));
    }

    #[test]
    fn local_key_wins_over_the_overlay() {
        let overlay = overlay_with(&[("GROQ_API_KEY", "gsk-vault")]);
        let p = OverlayProvider::new("groq", overlay, env_with(&[("GROQ_API_KEY", "gsk-local")]))
            .unwrap();
        assert_eq!(p.lookup("GROQ_API_KEY").as_deref(), Some("gsk-local"));
        assert!(p.current().is_ok());
    }

    #[test]
    fn empty_local_key_does_not_hide_the_overlay() {
        let p = OverlayProvider::new(
            "groq",
            overlay_with(&[("GROQ_API_KEY", "gsk-vault")]),
            env_with(&[("GROQ_API_KEY", "")]),
        )
        .unwrap();
        assert_eq!(p.lookup("GROQ_API_KEY").as_deref(), Some("gsk-vault"));
    }

    #[test]
    fn provider_is_rebuilt_only_when_the_key_changes() {
        let overlay = overlay_with(&[("GROQ_API_KEY", "gsk-1")]);
        let p = OverlayProvider::new("groq", overlay.clone(), env_with(&[])).unwrap();
        let first = p.current().unwrap();
        let again = p.current().unwrap();
        assert!(Arc::ptr_eq(&first, &again), "same key, same provider");
        overlay.replace(
            [("GROQ_API_KEY".to_string(), "gsk-2".to_string())]
                .into_iter()
                .collect(),
        );
        let rotated = p.current().unwrap();
        assert!(!Arc::ptr_eq(&first, &rotated), "a new key rebuilds");
    }

    #[test]
    fn a_removed_vault_key_stops_the_next_turn() {
        let overlay = overlay_with(&[("GROQ_API_KEY", "gsk-1")]);
        let p = OverlayProvider::new("groq", overlay.clone(), env_with(&[])).unwrap();
        assert!(p.current().is_ok());
        overlay.replace(Default::default());
        let err = p.current().err().expect("the key is gone");
        assert!(missing_key_var(&err).unwrap().contains("GROQ_API_KEY"));
    }
}
