//! Provider credential store — shared by the CLI (`modron auth`) and the
//! TUI (`/auth`), so both read and write the same truth.
//!
//! Keys live as `export <VAR>="<key>"` lines in `~/.config/modron/env`
//! (0600; or `<MODRON_CONFIG>/env` when that override is set, matching the
//! theme-dir convention). First-run onboarding, `modron auth set`, and the
//! TUI `/auth <provider> <key>` command all write here; every launch path
//! (TUI, one-shot, daemon) sources the file into the process environment,
//! where shell exports still win.

/// One provider in the credential registry: what `--provider <id>`
/// expects and which env var its key unlocks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderInfo {
    pub name: &'static str,
    /// The `--provider` id.
    pub provider_id: &'static str,
    pub env_var: &'static str,
    /// Where the user gets a key.
    pub key_url: &'static str,
    /// One-line note (free tier, speed, …) shown by pickers.
    pub note: &'static str,
}

/// The known-provider registry. Shared by first-run onboarding, the
/// `modron auth` wizard, and the TUI `/auth` command.
pub fn provider_registry() -> Vec<ProviderInfo> {
    vec![
        ProviderInfo {
            name: "Z.ai (GLM)",
            provider_id: "zai",
            env_var: "ZAI_API_KEY",
            key_url: "https://z.ai/manage-apikey/apikey-list",
            note: "free tier available (glm-4.5-flash)",
        },
        ProviderInfo {
            name: "Anthropic (Claude)",
            provider_id: "anthropic",
            env_var: "ANTHROPIC_API_KEY",
            key_url: "https://console.anthropic.com",
            note: "Claude models",
        },
        ProviderInfo {
            name: "OpenAI",
            provider_id: "openai-compat",
            env_var: "OPENAI_API_KEY",
            key_url: "https://platform.openai.com/api-keys",
            note: "GPT models",
        },
        ProviderInfo {
            name: "Google Gemini",
            provider_id: "google",
            env_var: "GEMINI_API_KEY",
            key_url: "https://aistudio.google.com/apikey",
            note: "free tier (2.5 Flash, 1M context)",
        },
        ProviderInfo {
            name: "Groq",
            provider_id: "groq",
            env_var: "GROQ_API_KEY",
            key_url: "https://console.groq.com/keys",
            note: "free tier, fastest inference",
        },
        ProviderInfo {
            name: "OpenRouter",
            provider_id: "openrouter",
            env_var: "OPENROUTER_API_KEY",
            key_url: "https://openrouter.ai/keys",
            note: "400+ models, some free",
        },
        ProviderInfo {
            name: "Mistral",
            provider_id: "mistral",
            env_var: "MISTRAL_API_KEY",
            key_url: "https://console.mistral.ai",
            note: "free experiment tier",
        },
        ProviderInfo {
            name: "Cerebras",
            provider_id: "cerebras",
            env_var: "CEREBRAS_API_KEY",
            key_url: "https://cloud.cerebras.ai",
            note: "free tier, fast",
        },
        ProviderInfo {
            name: "Nvidia NIM",
            provider_id: "nvidia",
            env_var: "NVIDIA_API_KEY",
            key_url: "https://build.nvidia.com",
            note: "free tier",
        },
        ProviderInfo {
            name: "GitHub Models",
            provider_id: "github-models",
            env_var: "GITHUB_MODELS_TOKEN",
            key_url: "https://github.com/settings/personal-access-tokens",
            note: "free tier with a GitHub account",
        },
    ]
}

/// Registry lookup by provider id (`zai`, `groq`, …).
pub fn provider_info(provider_id: &str) -> Option<ProviderInfo> {
    provider_registry()
        .into_iter()
        .find(|p| p.provider_id == provider_id)
}

/// The env file where keys are saved.
pub fn env_file() -> std::io::Result<std::path::PathBuf> {
    if let Ok(dir) = std::env::var("MODRON_CONFIG") {
        return Ok(std::path::PathBuf::from(dir).join("env"));
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    Ok(std::path::PathBuf::from(home)
        .join(".config")
        .join("modron")
        .join("env"))
}

/// Parse every `export VAR="value"` line of an env file into pairs.
pub fn parse_env_file(content: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("export ") {
            if let Some((key, value)) = rest.split_once('=') {
                let value = value.trim().trim_matches('"');
                if !key.trim().is_empty() && !value.is_empty() {
                    out.push((key.trim().to_string(), value.to_string()));
                }
            }
        }
    }
    out
}

/// Write/replace one provider key in the env file (0600 on create).
pub fn set_key_in_env_file(
    path: &std::path::Path,
    env_var: &str,
    key: &str,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut lines: Vec<String> = existing
        .lines()
        .filter(|l| !l.starts_with(&format!("export {env_var}=")))
        .map(String::from)
        .collect();
    lines.push(format!("export {env_var}=\"{key}\""));
    std::fs::write(path, lines.join("\n") + "\n")?;
    chmod_600(path);
    Ok(())
}

/// Remove one provider's key line. Ok(false) = nothing was stored.
pub fn remove_key_from_env_file(path: &std::path::Path, env_var: &str) -> std::io::Result<bool> {
    let existing = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(_) => return Ok(false),
    };
    let lines: Vec<String> = existing
        .lines()
        .filter(|l| !l.starts_with(&format!("export {env_var}=")))
        .map(String::from)
        .collect();
    if lines.len() == existing.lines().count() {
        return Ok(false); // nothing matched
    }
    std::fs::write(path, lines.join("\n") + "\n")?;
    Ok(true)
}

#[cfg(unix)]
fn chmod_600(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}
#[cfg(not(unix))]
fn chmod_600(_path: &std::path::Path) {}

/// Mask a key for display: `sk-…wxyz` (fully hidden when very short).
pub fn mask_key(key: &str) -> String {
    let n = key.chars().count();
    if n <= 8 {
        return "…".to_string();
    }
    let head: String = key.chars().take(3).collect();
    let tail: String = key.chars().skip(n - 4).collect();
    format!("{head}…{tail}")
}

/// Where a configured key came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeySource {
    /// Saved in the env file (the normal path; sourced at startup).
    EnvFile,
    /// Exported in the shell only.
    ProcessEnv,
    /// Not configured.
    None,
}

/// Resolve a key and its source: the saved file wins for labeling
/// (startup sources it into the process anyway); only shell-only exports
/// show as process env.
pub fn key_source(env_var: &str) -> (KeySource, String) {
    if let Ok(path) = env_file() {
        if let Ok(content) = std::fs::read_to_string(&path) {
            for (var, value) in parse_env_file(&content) {
                if var == env_var {
                    return (KeySource::EnvFile, value);
                }
            }
        }
    }
    if let Ok(v) = std::env::var(env_var) {
        if !v.is_empty() {
            return (KeySource::ProcessEnv, v);
        }
    }
    (KeySource::None, String::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_ids_are_unique_and_env_vars_differ() {
        let reg = provider_registry();
        let mut ids: Vec<_> = reg.iter().map(|p| p.provider_id).collect();
        let total = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), total, "duplicate provider id");
        let mut vars: Vec<_> = reg.iter().map(|p| p.env_var).collect();
        vars.sort_unstable();
        vars.dedup();
        assert_eq!(vars.len(), total, "duplicate env var");
        assert!(reg.len() >= 10);
    }

    #[test]
    fn env_file_set_parse_remove_roundtrip() {
        let dir = std::env::temp_dir().join(format!("modron-core-auth-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("env");

        set_key_in_env_file(&path, "ZAI_API_KEY", "sk-first").unwrap();
        set_key_in_env_file(&path, "GROQ_API_KEY", "gsk_second").unwrap();
        // Re-setting replaces, never duplicates.
        set_key_in_env_file(&path, "ZAI_API_KEY", "sk-replaced").unwrap();

        let pairs = parse_env_file(&std::fs::read_to_string(&path).unwrap());
        assert_eq!(pairs.len(), 2, "{pairs:?}");
        assert!(pairs.contains(&("ZAI_API_KEY".into(), "sk-replaced".into())));
        assert!(pairs.contains(&("GROQ_API_KEY".into(), "gsk_second".into())));

        assert!(remove_key_from_env_file(&path, "GROQ_API_KEY").unwrap());
        assert!(!remove_key_from_env_file(&path, "GROQ_API_KEY").unwrap());
        assert_eq!(
            parse_env_file(&std::fs::read_to_string(&path).unwrap()).len(),
            1
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keys_are_masked_for_display() {
        assert_eq!(mask_key("sk-ant-1234567890"), "sk-…7890");
        assert_eq!(mask_key("short"), "…");
    }

    #[test]
    fn provider_lookup_finds_and_rejects() {
        assert_eq!(provider_info("zai").unwrap().env_var, "ZAI_API_KEY");
        assert!(provider_info("nope").is_none());
    }
}
