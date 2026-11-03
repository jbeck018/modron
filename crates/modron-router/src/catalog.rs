//! R2 — embedded model catalog: context windows, approximate $/1M pricing,
//! and a cheap/strong tier per model.
//!
//! The catalog lives in a compiled-in TOML string and is parsed once into a
//! static slice. It exists for pre-flight cost estimates, overflow
//! escalation in the [`crate::selector`], and the CLI's cost table — it is
//! **not** billing data.

use serde::Deserialize;
use std::sync::OnceLock;

/// Approximate pricing/context data, reviewed quarterly (last: 2026-10).
/// Deliberately a plain string: zero runtime I/O, works offline.
const CATALOG_TOML: &str = r#"
# Modron model catalog — APPROXIMATE pricing, reviewed quarterly (last: 2026-10).
# Fields: name, provider, context_window (tokens), input_per_m / output_per_m
# (USD per 1M tokens; 0 for local models), tier ("cheap" | "strong").

# Anthropic lineup, verified against the subscription's own
# GET /v1/models + the official pricing page (2026-11-02). Claude 4.6+
# models carry a 1M window at standard pricing (except haiku-5-5, which
# price-tiers at the 100k prompt boundary).
[[models]]
name = "claude-opus-5-5"
provider = "anthropic"
context_window = 1_000_000
input_per_m = 4.0
output_per_m = 20.0
tier = "strong"

[[models]]
name = "claude-sonnet-5-5"
provider = "anthropic"
context_window = 1_000_000
input_per_m = 2.0
output_per_m = 10.0
tier = "strong"

[[models]]
name = "claude-haiku-5-5"
provider = "anthropic"
context_window = 200_000
input_per_m = 0.10
output_per_m = 0.50
tier = "cheap"

[[models]]
name = "claude-fable-5-1"
provider = "anthropic"
context_window = 1_000_000
input_per_m = 10.0
output_per_m = 50.0
tier = "strong"

# ChatGPT subscription models (OAuth sign-in, plan-billed: 0/0 pricing).
# The lineup is whatever /backend-api/codex/models returns for the signed
# in account (verified live 2026-11-02); other names 400 with
# "model is not supported when using Codex with a ChatGPT account".
[[models]]
name = "gpt-6-astra"
provider = "chatgpt"
context_window = 400_000
input_per_m = 0.0
output_per_m = 0.0
tier = "strong"

[[models]]
name = "gpt-5.6-sol"
provider = "chatgpt"
context_window = 400_000
input_per_m = 0.0
output_per_m = 0.0
tier = "strong"

[[models]]
name = "gpt-5.6-terra"
provider = "chatgpt"
context_window = 400_000
input_per_m = 0.0
output_per_m = 0.0
tier = "cheap"

[[models]]
name = "gpt-5.6-luna"
provider = "chatgpt"
context_window = 400_000
input_per_m = 0.0
output_per_m = 0.0
tier = "cheap"

# OpenAI API-key lineup (2026-11): gpt-6 family on api.openai.com, reached
# through the openai-compat provider (OPENAI_API_KEY) — no factory arm ever
# builds a provider registered as "openai", so catalog entries must not use
# that id. gpt-6-astra's name belongs to the chatgpt subscription entry
# above (names are unique catalog-wide); with an OPENAI_API_KEY it still
# works via `--provider openai-compat --model gpt-6-astra` (forced provider).
[[models]]
name = "gpt-6-sol"
provider = "openai-compat"
context_window = 400_000
input_per_m = 2.0
output_per_m = 10.0
tier = "strong"

[[models]]
name = "gpt-5.6-cyber"
provider = "openai-compat"
context_window = 400_000
input_per_m = 12.5
output_per_m = 75.0
tier = "strong"

[[models]]
name = "gemini-2.5-pro"
provider = "google"
context_window = 1_000_000
input_per_m = 1.25
output_per_m = 10.0
tier = "strong"

[[models]]
name = "gemini-2.5-flash"
provider = "google"
context_window = 1_000_000
input_per_m = 0.30
output_per_m = 2.50
tier = "cheap"   # AI Studio free tier: ~500 req/day at $0 (verified 2026-10)

[[models]]
name = "deepseek-chat"
provider = "deepseek"
context_window = 128_000
input_per_m = 0.27
output_per_m = 1.10
tier = "cheap"

[[models]]
name = "deepseek-reasoner"
provider = "deepseek"
context_window = 128_000
input_per_m = 0.55
output_per_m = 2.19
tier = "strong"

# Z.ai (Zhipu GLM) — model ids, 1M context windows and $/1M rates from
# docs.z.ai (pricing + model guide pages), reviewed 2026-10. Pricing is
# pay-as-you-go USD; Coding Plan subscribers pay subscription credits instead.
# Flagship = strong (agentic/coding), Flash/FlashX = cheap everyday tiers.

[[models]]
name = "glm-5.3"
provider = "zai"
context_window = 1_000_000
input_per_m = 1.40
output_per_m = 4.40
tier = "strong"

[[models]]
name = "glm-5.2"
provider = "zai"
context_window = 1_000_000
input_per_m = 1.40
output_per_m = 4.40
tier = "strong"

[[models]]
name = "glm-5.3-flash"
provider = "zai"
context_window = 1_000_000
input_per_m = 0.15
output_per_m = 0.50
tier = "cheap"

[[models]]
name = "glm-5.3-flashx"
provider = "zai"
context_window = 1_000_000
input_per_m = 0.37
output_per_m = 1.25
tier = "cheap"

[[models]]
name = "glm-4.7-flash"
provider = "zai"
context_window = 200000
input_per_m = 0.0
output_per_m = 0.0
tier = "cheap"   # free tier, verified live 2026-10-31

[[models]]
name = "glm-4.5-flash"
provider = "zai"
context_window = 128000
input_per_m = 0.0
output_per_m = 0.0
tier = "cheap"   # free tier

[[models]]
name = "llama-3.1-8b-instant"
provider = "groq"
context_window = 128000
input_per_m = 0.0
output_per_m = 0.0
tier = "cheap"   # Groq free tier

[[models]]
name = "llama-3.3-70b-versatile"
provider = "groq"
context_window = 128000
input_per_m = 0.0
output_per_m = 0.0
tier = "strong"  # Groq free tier; strongest free instruction-follower

[[models]]
name = "qwen-3-32b"
provider = "cerebras"
context_window = 128000
input_per_m = 0.0
output_per_m = 0.0
tier = "strong"  # Cerebras free tier; fastest tokens/sec

[[models]]
name = "mistral-small-latest"
provider = "mistral"
context_window = 128000
input_per_m = 0.0
output_per_m = 0.0
tier = "cheap"   # Mistral free experiment tier (data-training caveat)

[[models]]
name = "openai/gpt-4.1-mini"
provider = "github-models"
context_window = 128000
input_per_m = 0.0
output_per_m = 0.0
tier = "strong"  # GitHub Models free tier (per-minute/day caps)

[[models]]
name = "deepseek/deepseek-chat-v3-0324:free"
provider = "openrouter"
context_window = 128000
input_per_m = 0.0
output_per_m = 0.0
tier = "strong"  # OpenRouter :free (50 req/day basic)

[[models]]
name = "meta/llama-3.3-70b-instruct"
provider = "nvidia"
context_window = 128000
input_per_m = 0.0
output_per_m = 0.0
tier = "strong"  # NVIDIA NIM free credits

[[models]]
name = "llama-3.3-70b"
provider = "ollama"
context_window = 128_000
input_per_m = 0.0
output_per_m = 0.0
tier = "cheap"
"#;

/// One model in the catalog. Pricing is approximate — see [`catalog`].
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ModelInfo {
    /// Canonical model name as sent to the provider, e.g. "claude-haiku-4-5".
    pub name: String,
    /// Provider id: "anthropic" | "openai" | "openai-compat" | "google" |
    /// "deepseek" | "ollama" | "zai".
    pub provider: String,
    /// Advertised context window in tokens.
    pub context_window: u64,
    /// Approximate USD per 1M input tokens (0 for local models).
    pub input_per_m: f64,
    /// Approximate USD per 1M output tokens (0 for local models).
    pub output_per_m: f64,
    /// Routing tier: cheap models do mechanical work, strong ones do judgment.
    pub tier: Tier,
}

/// Routing tier of a model. Invalid tier strings in the embedded TOML are a
/// parse error caught by [`catalog`]'s startup check, so the enum is always
/// valid at runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Cheap,
    Strong,
}

impl Tier {
    /// Machine-readable tier name (used in routing `reason` strings).
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Cheap => "cheap",
            Tier::Strong => "strong",
        }
    }
}

#[derive(Deserialize)]
struct CatalogFile {
    #[serde(default)]
    models: Vec<ModelInfo>,
}

static CATALOG: OnceLock<Vec<ModelInfo>> = OnceLock::new();

fn parse_catalog() -> Vec<ModelInfo> {
    match toml::from_str::<CatalogFile>(CATALOG_TOML) {
        Ok(file) => file.models,
        Err(error) => {
            // The catalog is compiled in, so a parse failure is a bug rather
            // than a runtime condition. Fail soft (empty catalog, lookups
            // return None) and say so loudly; never panic in library code.
            tracing::error!(%error, "embedded model catalog failed to parse; model lookup disabled");
            Vec::new()
        }
    }
}

/// Approximate pricing, reviewed quarterly. Returns the whole static catalog
/// (parsed once, then shared). Order is the TOML order; callers that need a
/// specific model should use [`lookup`].
pub fn catalog() -> &'static [ModelInfo] {
    CATALOG.get_or_init(parse_catalog)
}

/// Look up one model by canonical name, e.g. `lookup("claude-haiku-4-5")`.
/// Returns `None` for unknown models (and for an empty catalog if the
/// embedded TOML ever failed to parse — see [`catalog`]).
pub fn lookup(name: &str) -> Option<&'static ModelInfo> {
    catalog().iter().find(|m| m.name == name)
}

impl ModelInfo {
    /// Estimated USD cost for the given token counts at this model's
    /// approximate rates. Exact for round inputs (e.g. 1M tokens at $3/M is
    /// exactly 3.0); an estimate everywhere else — never billing data.
    pub fn cost_usd(&self, input_tokens: u64, output_tokens: u64) -> f64 {
        (input_tokens as f64 / 1_000_000.0) * self.input_per_m
            + (output_tokens as f64 / 1_000_000.0) * self.output_per_m
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALLOWED_PROVIDERS: [&str; 13] = [
        "anthropic",
        "openai-compat",
        "google",
        "deepseek",
        "ollama",
        "zai",
        "groq",
        "cerebras",
        "mistral",
        "nvidia",
        "openrouter",
        "github-models",
        "chatgpt",
    ];

    #[test]
    fn lookup_known_model() {
        let m = lookup("claude-opus-5-5").expect("opus should be in catalog");
        assert_eq!(m.provider, "anthropic");
        assert_eq!(m.tier, Tier::Strong);
        assert_eq!(m.context_window, 1_000_000);
    }

    #[test]
    fn lookup_unknown_model_is_none() {
        assert!(lookup("gpt-99-turbo").is_none());
        assert!(lookup("").is_none());
    }

    #[test]
    fn cost_math_is_exact() {
        let m = ModelInfo {
            name: "test-model".into(),
            provider: "openai".into(),
            context_window: 128_000,
            input_per_m: 3.0,
            output_per_m: 15.0,
            tier: Tier::Strong,
        };
        assert_eq!(m.cost_usd(0, 0), 0.0);
        assert_eq!(m.cost_usd(1_000_000, 0), 3.0);
        assert_eq!(m.cost_usd(0, 1_000_000), 15.0);
        assert_eq!(m.cost_usd(1_000_000, 1_000_000), 18.0);
        assert_eq!(m.cost_usd(500_000, 2_000_000), 31.5);
    }

    #[test]
    fn catalog_cost_matches_published_rate() {
        // platform.claude.com pricing page (2026-11): opus-5-5 $4/$20.
        let opus = lookup("claude-opus-5-5").expect("in catalog");
        assert_eq!(opus.cost_usd(1_000_000, 0), 4.0);
        assert_eq!(opus.cost_usd(0, 1_000_000), 20.0);
    }

    #[test]
    fn catalog_has_at_least_twelve_valid_entries() {
        let cat = catalog();
        assert!(cat.len() >= 12, "catalog has only {} models", cat.len());
        for m in cat {
            assert!(
                ALLOWED_PROVIDERS.contains(&m.provider.as_str()),
                "provider `{}` not in allowed set",
                m.provider
            );
            assert!(m.context_window > 0, "zero context window for {}", m.name);
            assert!(m.input_per_m >= 0.0, "negative input price for {}", m.name);
            assert!(
                m.output_per_m >= 0.0,
                "negative output price for {}",
                m.name
            );
            assert!(matches!(m.tier, Tier::Cheap | Tier::Strong));
        }
        let mut names: Vec<&str> = cat.iter().map(|m| m.name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), cat.len(), "model names must be unique");
    }

    #[test]
    fn every_required_model_is_present() {
        for name in [
            "claude-opus-5-5",
            "claude-sonnet-5-5",
            "claude-haiku-5-5",
            "claude-fable-5-1",
            "gpt-6-astra",
            "gpt-5.6-sol",
            "gpt-6-sol",
            "gpt-5.6-cyber",
            "gemini-2.5-pro",
            "gemini-2.5-flash",
            "deepseek-chat",
            "deepseek-reasoner",
            "llama-3.3-70b",
            "glm-5.3",
            "glm-5.2",
            "glm-5.3-flash",
            "glm-5.3-flashx",
        ] {
            assert!(lookup(name).is_some(), "missing {name}");
        }
    }

    #[test]
    fn glm_flagship_entries_are_zai_strong_with_documented_window_and_price() {
        // docs.z.ai (2026-10): glm-5.3 and glm-5.2, 1M context, $1.40/$4.40
        // per 1M (cached input $0.26 — not a catalog field).
        for name in ["glm-5.3", "glm-5.2"] {
            let m = lookup(name).unwrap_or_else(|| panic!("{name} should be in catalog"));
            assert_eq!(m.provider, "zai");
            assert_eq!(m.tier, Tier::Strong, "{name} is flagship → strong");
            assert_eq!(m.context_window, 1_000_000, "{name} documented window");
            assert_eq!(m.input_per_m, 1.40, "{name} documented input rate");
            assert_eq!(m.output_per_m, 4.40, "{name} documented output rate");
        }
        let glm = lookup("glm-5.3").expect("in catalog");
        assert_eq!(glm.cost_usd(1_000_000, 0), 1.40);
        assert_eq!(glm.cost_usd(0, 1_000_000), 4.40);
    }

    #[test]
    fn glm_flash_entries_are_zai_cheap_with_documented_window_and_price() {
        // docs.z.ai (2026-10): glm-5.3-flash $0.15/$0.50, glm-5.3-flashx
        // $0.37/$1.25; both 1M context. Flash is the cheap everyday tier.
        let flash = lookup("glm-5.3-flash").expect("in catalog");
        assert_eq!(flash.provider, "zai");
        assert_eq!(flash.tier, Tier::Cheap);
        assert_eq!(flash.context_window, 1_000_000);
        assert_eq!(flash.input_per_m, 0.15);
        assert_eq!(flash.output_per_m, 0.50);

        let flashx = lookup("glm-5.3-flashx").expect("in catalog");
        assert_eq!(flashx.provider, "zai");
        assert_eq!(flashx.tier, Tier::Cheap);
        assert_eq!(flashx.context_window, 1_000_000);
        assert_eq!(flashx.input_per_m, 0.37);
        assert_eq!(flashx.output_per_m, 1.25);
    }

    #[test]
    fn local_ollama_model_is_free() {
        let llama = lookup("llama-3.3-70b").expect("in catalog");
        assert_eq!(llama.provider, "ollama");
        assert_eq!(llama.cost_usd(10_000_000, 10_000_000), 0.0);
    }
}
