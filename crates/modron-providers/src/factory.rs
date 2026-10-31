//! Provider factory — LEDGER item P3.
//!
//! Builds providers from a config source with actionable errors: a missing
//! API key yields [`Error::Config`] naming the env var and the fix. The `env`
//! lookup is a closure so unit tests never mutate process-global state; the
//! convenience constructor reads `std::env` directly.

use crate::{AnthropicProvider, OpenAiCompatProvider};
use modron_core::{Error, ModelProvider, Result};
use std::sync::Arc;

/// Z.ai (Zhipu) international pay-as-you-go endpoint — OpenAI-compatible
/// (`POST {base}/chat/completions`). The China-mainland mirror is
/// `https://open.bigmodel.cn/api/paas/v4`; either can be selected via
/// `ZAI_BASE_URL`. GLM Coding Plan subscriptions have their *own*
/// OpenAI-compatible endpoint (`https://api.z.ai/api/coding/paas/v4`) and
/// their keys are explicitly NOT interchangeable with standard API keys —
/// a coding-plan key on this endpoint gets a 401.
const DEFAULT_ZAI_BASE_URL: &str = "https://api.z.ai/api/paas/v4";

/// Fix hint for a missing [`crate::factory`] Z.ai key. Points at the standard
/// (pay-as-you-go) key console and names the coding-plan trap up front.
const ZAI_KEY_HINT: &str = "get a pay-as-you-go key at https://z.ai/manage-apikey/apikey-list \
     — GLM Coding Plan keys are NOT interchangeable (they only work on the \
     api/coding/paas/v4 and api/anthropic endpoints)";

/// Build a provider from process environment variables. Empty variables are
/// treated as unset.
pub fn provider_from_env(provider_id: &str) -> Result<Arc<dyn ModelProvider>> {
    provider_from_config(provider_id, |key| {
        std::env::var(key).ok().filter(|value| !value.is_empty())
    })
}

/// Build a provider from a config source. `env` is the lookup seam — pass a
/// closure over process env, a parsed config file, or a test fixture.
///
/// Supported ids: `"anthropic"` (env: `ANTHROPIC_API_KEY`, optional
/// `ANTHROPIC_BASE_URL`), `"openai-compat"` (env: `OPENAI_API_KEY`, optional
/// `OPENAI_BASE_URL`; the key may be omitted entirely when a custom base URL
/// is set, so keyless local servers like Ollama work), and `"zai"` (env:
/// `ZAI_API_KEY`, optional `ZAI_BASE_URL` override for the bigmodel.cn mirror
/// or the coding-plan endpoint; the key is always required). The scripted
/// `"mock"` provider cannot be built from config — construct
/// `modron_core::testing::MockProvider` and register it directly.
/// Free-tier OpenAI-compatible providers (Phase 5g): key from `env_key`,
/// hosted base URL by default, `base_env_key` overrides.
fn openai_compat_with_key(
    env: &impl Fn(&str) -> Option<String>,
    env_key: &str,
    default_base: &str,
    key_hint: &str,
    base_env_key: &str,
) -> Result<Arc<dyn ModelProvider>> {
    let api_key = env(env_key).ok_or_else(|| missing_key(env_key, key_hint))?;
    let mut provider = OpenAiCompatProvider::new(api_key)?;
    if let Some(base_url) = env(base_env_key) {
        provider = provider.with_base_url(base_url);
    } else {
        provider = provider.with_base_url(default_base);
    }
    Ok(Arc::new(provider))
}

pub fn provider_from_config(
    provider_id: &str,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Arc<dyn ModelProvider>> {
    match provider_id {
        "anthropic" => {
            let api_key = env("ANTHROPIC_API_KEY").ok_or_else(|| {
                missing_key(
                    "ANTHROPIC_API_KEY",
                    "get a key at https://console.anthropic.com",
                )
            })?;
            let mut provider = AnthropicProvider::new(api_key)?;
            if let Some(base_url) = env("ANTHROPIC_BASE_URL") {
                provider = provider.with_base_url(base_url);
            }
            Ok(Arc::new(provider))
        }
        "openai-compat" => {
            let base_url = env("OPENAI_BASE_URL");
            match env("OPENAI_API_KEY") {
                Some(api_key) => {
                    let mut provider = OpenAiCompatProvider::new(api_key)?;
                    if let Some(base_url) = base_url {
                        provider = provider.with_base_url(base_url);
                    }
                    Ok(Arc::new(provider))
                }
                // Keyless local endpoints (Ollama, llama.cpp, vLLM) ignore auth.
                None if base_url.is_some() => {
                    let provider = OpenAiCompatProvider::new(String::new())?
                        .with_base_url(base_url.unwrap_or_default());
                    Ok(Arc::new(provider))
                }
                None => Err(missing_key(
                    "OPENAI_API_KEY",
                    "or point OPENAI_BASE_URL at a local server (e.g. http://localhost:11434/v1 for Ollama), which needs no key",
                )),
            }
        }
        "zai" => Ok(Arc::new(zai_provider(&env)?)),
        // Free-tier providers (Phase 5g): all OpenAI-compatible; each is a
        // base URL + env key. See README "Free models" for rate limits.
        "groq" => openai_compat_with_key(
            &env,
            "GROQ_API_KEY",
            "https://api.groq.com/openai/v1",
            "https://console.groq.com/keys",
            "GROQ_BASE_URL",
        ),
        "google" => openai_compat_with_key(
            &env,
            "GEMINI_API_KEY",
            "https://generativelanguage.googleapis.com/v1beta/openai",
            "https://aistudio.google.com/apikey",
            "GOOGLE_BASE_URL",
        ),
        "cerebras" => openai_compat_with_key(
            &env,
            "CEREBRAS_API_KEY",
            "https://api.cerebras.ai/v1",
            "https://cloud.cerebras.ai",
            "CEREBRAS_BASE_URL",
        ),
        "mistral" => openai_compat_with_key(
            &env,
            "MISTRAL_API_KEY",
            "https://api.mistral.ai/v1",
            "https://console.mistral.ai",
            "MISTRAL_BASE_URL",
        ),
        "nvidia" => openai_compat_with_key(
            &env,
            "NVIDIA_API_KEY",
            "https://integrate.api.nvidia.com/v1",
            "https://build.nvidia.com",
            "NVIDIA_BASE_URL",
        ),
        "openrouter" => openai_compat_with_key(
            &env,
            "OPENROUTER_API_KEY",
            "https://openrouter.ai/api/v1",
            "https://openrouter.ai/keys",
            "OPENROUTER_BASE_URL",
        ),
        "github-models" => openai_compat_with_key(
            &env,
            "GITHUB_MODELS_TOKEN",
            "https://models.github.ai/inference",
            "https://github.com/settings/personal-access-tokens",
            "GITHUB_MODELS_BASE_URL",
        ),
        "mock" => Err(Error::Config(
            "mock provider cannot be built from config".into(),
            "construct modron_core::testing::MockProvider with a script and register it \
             directly in Engine.providers under the key \"mock\""
                .into(),
        )),
        other => Err(Error::Config(
            format!("unknown provider `{other}`"),
            "supported providers: anthropic, openai-compat, zai".into(),
        )),
    }
}

/// Build the Z.ai provider: an [`OpenAiCompatProvider`] against
/// [`DEFAULT_ZAI_BASE_URL`], key from `ZAI_API_KEY`, endpoint overridable via
/// `ZAI_BASE_URL` (bigmodel.cn mirror or the coding-plan endpoint). Unlike
/// `openai-compat` there is no keyless path — both Z.ai endpoints are hosted
/// services that require auth. Returns the concrete type so tests can assert
/// the mapping through [`OpenAiCompatProvider::base_url`].
fn zai_provider(env: &impl Fn(&str) -> Option<String>) -> Result<OpenAiCompatProvider> {
    let api_key = env("ZAI_API_KEY").ok_or_else(|| missing_key("ZAI_API_KEY", ZAI_KEY_HINT))?;
    let base_url = env("ZAI_BASE_URL").unwrap_or_else(|| DEFAULT_ZAI_BASE_URL.to_string());
    Ok(OpenAiCompatProvider::new(api_key)?.with_base_url(base_url))
}

/// The canonical missing-key error: names the env var and shows the fix.
fn missing_key(var: &str, hint: &str) -> Error {
    Error::Config(format!("missing {var}"), format!("export {var}=… ({hint})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_with<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            vars.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.to_string())
        }
    }

    /// `Result::unwrap_err` needs the success type to be `Debug`; a provider
    /// is not. Match instead.
    fn config_err(result: Result<Arc<dyn ModelProvider>>) -> Error {
        match result {
            Err(err) => err,
            Ok(_) => panic!("expected Err(Error::Config), got Ok"),
        }
    }

    #[test]
    fn missing_anthropic_key_is_actionable() {
        let err = config_err(provider_from_config("anthropic", |_| None));
        match err {
            Error::Config(what, hint) => {
                assert!(
                    what.contains("ANTHROPIC_API_KEY"),
                    "names the env var: {what}"
                );
                assert!(
                    hint.contains("export ANTHROPIC_API_KEY"),
                    "shows the fix: {hint}"
                );
            }
            other => panic!("expected Error::Config, got: {other}"),
        }
    }

    #[test]
    fn anthropic_from_closure_env() {
        let provider = provider_from_config("anthropic", |key| {
            (key == "ANTHROPIC_API_KEY").then(|| "sk-test".to_string())
        })
        .unwrap();
        assert_eq!(provider.id(), "anthropic");
    }

    #[test]
    fn anthropic_base_url_override() {
        // Exercises the ANTHROPIC_BASE_URL plumbing; the resulting client's
        // base_url is checked via the concrete type in the anthropic tests.
        let provider = provider_from_config(
            "anthropic",
            env_with(&[
                ("ANTHROPIC_API_KEY", "sk-test"),
                ("ANTHROPIC_BASE_URL", "http://localhost:8082"),
            ]),
        )
        .unwrap();
        assert_eq!(provider.id(), "anthropic");
    }

    #[test]
    fn openai_missing_key_and_base_is_actionable() {
        let err = config_err(provider_from_config("openai-compat", |_| None));
        match err {
            Error::Config(what, hint) => {
                assert!(what.contains("OPENAI_API_KEY"), "names the env var: {what}");
                assert!(
                    hint.contains("export OPENAI_API_KEY"),
                    "shows the fix: {hint}"
                );
            }
            other => panic!("expected Error::Config, got: {other}"),
        }
    }

    #[test]
    fn openai_key_with_base_override() {
        let provider = provider_from_config(
            "openai-compat",
            env_with(&[
                ("OPENAI_API_KEY", "sk-test"),
                ("OPENAI_BASE_URL", "https://openrouter.ai/api/v1"),
            ]),
        )
        .unwrap();
        assert_eq!(provider.id(), "openai-compat");
    }

    #[test]
    fn openai_custom_base_allows_missing_key() {
        // Ollama and friends ignore the key entirely.
        let provider = provider_from_config(
            "openai-compat",
            env_with(&[("OPENAI_BASE_URL", "http://localhost:11434/v1")]),
        )
        .unwrap();
        assert_eq!(provider.id(), "openai-compat");
    }

    #[test]
    fn zai_missing_key_is_actionable() {
        let err = config_err(provider_from_config("zai", |_| None));
        match err {
            Error::Config(what, hint) => {
                assert!(what.contains("ZAI_API_KEY"), "names the env var: {what}");
                assert!(hint.contains("export ZAI_API_KEY"), "shows the fix: {hint}");
                // The coding-plan trap is the #1 support issue: subscription
                // keys get 401 on the pay-as-you-go endpoint.
                assert!(
                    hint.contains("Coding Plan"),
                    "warns about coding-plan keys: {hint}"
                );
            }
            other => panic!("expected Error::Config, got: {other}"),
        }
    }

    #[test]
    fn zai_maps_to_openai_compat_with_default_base_url() {
        let provider =
            zai_provider(&|key| (key == "ZAI_API_KEY").then(|| "zai-test".to_string())).unwrap();
        // Mapping: Z.ai is served by the OpenAI-compatible client against the
        // international pay-as-you-go base URL.
        assert_eq!(provider.base_url(), "https://api.z.ai/api/paas/v4");

        // Through the public factory the same holds; the concrete type's
        // `id()` is the openai-compat constant — `zai` is a configured
        // endpoint of that provider kind, not a distinct wire format. (If the
        // concrete type ever grows a display-name knob, update this assert.)
        let provider = provider_from_config("zai", |key| {
            (key == "ZAI_API_KEY").then(|| "zai-test".to_string())
        })
        .unwrap();
        assert_eq!(provider.id(), "openai-compat");
    }

    #[test]
    fn zai_base_url_override_wins() {
        // The bigmodel.cn China-mainland mirror …
        let provider = zai_provider(&env_with(&[
            ("ZAI_API_KEY", "zai-test"),
            ("ZAI_BASE_URL", "https://open.bigmodel.cn/api/paas/v4"),
        ]))
        .unwrap();
        assert_eq!(provider.base_url(), "https://open.bigmodel.cn/api/paas/v4");

        // … and the coding-plan endpoint, for coding-plan key holders.
        let provider = zai_provider(&env_with(&[
            ("ZAI_API_KEY", "zai-test"),
            ("ZAI_BASE_URL", "https://api.z.ai/api/coding/paas/v4"),
        ]))
        .unwrap();
        assert_eq!(provider.base_url(), "https://api.z.ai/api/coding/paas/v4");
    }

    #[test]
    fn zai_missing_key_is_not_keyless_even_with_base_override() {
        // Unlike openai-compat (keyless local servers), both Z.ai endpoints
        // are hosted services that always require auth.
        let err = match zai_provider(&|key| {
            (key == "ZAI_BASE_URL").then(|| "https://open.bigmodel.cn/api/paas/v4".to_string())
        }) {
            Ok(_) => panic!("expected Err(Error::Config), got Ok"),
            Err(err) => err,
        };
        match err {
            Error::Config(what, _) => {
                assert!(what.contains("ZAI_API_KEY"), "names the env var: {what}");
            }
            other => panic!("expected Error::Config, got: {other}"),
        }
    }

    #[test]
    fn mock_provider_is_not_buildable_from_config() {
        let err = config_err(provider_from_config("mock", |_| None));
        match err {
            Error::Config(what, hint) => {
                assert!(what.contains("mock"));
                assert!(
                    hint.contains("MockProvider"),
                    "points at the in-process type: {hint}"
                );
            }
            other => panic!("expected Error::Config, got: {other}"),
        }
    }

    #[test]
    fn unknown_provider_names_the_supported_set() {
        let err = config_err(provider_from_config("bogus", |_| None));
        match err {
            Error::Config(what, hint) => {
                assert!(what.contains("bogus"));
                assert!(
                    hint.contains("anthropic")
                        && hint.contains("openai-compat")
                        && hint.contains("zai")
                );
            }
            other => panic!("expected Error::Config, got: {other}"),
        }
    }
}
