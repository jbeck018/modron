//! First-run onboarding (Phase 12): when no provider keys exist, the TUI
//! shows a friendly setup screen instead of erroring. The user picks a
//! provider, pastes a key, and it's saved to `~/.config/modron/env` (0600)
//! — sourced automatically on every subsequent launch.

use std::io::Write;
use std::path::PathBuf;

/// One provider option on the onboarding screen (and the auth wizard).
pub struct ProviderOption {
    pub number: u8,
    pub name: &'static str,
    /// The `--provider` id the key unlocks.
    pub provider_id: &'static str,
    pub env_var: &'static str,
    pub key_url: &'static str,
    pub note: &'static str,
}

pub fn provider_options() -> Vec<ProviderOption> {
    vec![
        ProviderOption {
            number: 1,
            name: "Z.ai (GLM)",
            provider_id: "zai",
            env_var: "ZAI_API_KEY",
            key_url: "https://z.ai/manage-apikey/apikey-list",
            note: "free tier available (glm-4.5-flash)",
        },
        ProviderOption {
            number: 2,
            name: "Anthropic (Claude)",
            provider_id: "anthropic",
            env_var: "ANTHROPIC_API_KEY",
            key_url: "https://console.anthropic.com",
            note: "Claude models",
        },
        ProviderOption {
            number: 3,
            name: "OpenAI",
            provider_id: "openai-compat",
            env_var: "OPENAI_API_KEY",
            key_url: "https://platform.openai.com/api-keys",
            note: "GPT models",
        },
        ProviderOption {
            number: 4,
            name: "Google Gemini",
            provider_id: "google",
            env_var: "GEMINI_API_KEY",
            key_url: "https://aistudio.google.com/apikey",
            note: "free tier (2.5 Flash, 1M context)",
        },
        ProviderOption {
            number: 5,
            name: "Groq",
            provider_id: "groq",
            env_var: "GROQ_API_KEY",
            key_url: "https://console.groq.com/keys",
            note: "free tier, fastest inference",
        },
        ProviderOption {
            number: 6,
            name: "OpenRouter",
            provider_id: "openrouter",
            env_var: "OPENROUTER_API_KEY",
            key_url: "https://openrouter.ai/keys",
            note: "400+ models, some free",
        },
        ProviderOption {
            number: 7,
            name: "Mistral",
            provider_id: "mistral",
            env_var: "MISTRAL_API_KEY",
            key_url: "https://console.mistral.ai",
            note: "free experiment tier",
        },
        ProviderOption {
            number: 8,
            name: "Cerebras",
            provider_id: "cerebras",
            env_var: "CEREBRAS_API_KEY",
            key_url: "https://cloud.cerebras.ai",
            note: "free tier, fast",
        },
        ProviderOption {
            number: 9,
            name: "Nvidia NIM",
            provider_id: "nvidia",
            env_var: "NVIDIA_API_KEY",
            key_url: "https://build.nvidia.com",
            note: "free tier",
        },
        ProviderOption {
            number: 10,
            name: "GitHub Models",
            provider_id: "github-models",
            env_var: "GITHUB_MODELS_TOKEN",
            key_url: "https://github.com/settings/personal-access-tokens",
            note: "free tier with a GitHub account",
        },
    ]
}

/// Find a registry entry by provider id (`zai`, `anthropic`, …).
pub fn option_for_provider(provider_id: &str) -> Option<ProviderOption> {
    provider_options()
        .into_iter()
        .find(|o| o.provider_id == provider_id)
}

/// The env file where onboarding saves keys: `~/.config/modron/env`
/// (or `<MODRON_CONFIG>/env` when that override is set, matching the
/// theme dir convention).
pub fn env_file() -> anyhow::Result<PathBuf> {
    if let Ok(dir) = std::env::var("MODRON_CONFIG") {
        return Ok(PathBuf::from(dir).join("env"));
    }
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
    Ok(home.join(".config").join("modron").join("env"))
}

/// Check if ANY provider key is available (env vars or the saved env file).
pub fn has_any_key() -> bool {
    // Source the saved env file first.
    source_env_file();
    provider_options().iter().any(|p| {
        std::env::var(p.env_var)
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    })
}

/// Source the saved env file into the process environment (idempotent).
pub fn source_env_file() {
    let Ok(path) = env_file() else { return };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return;
    };
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("export ") {
            if let Some((key, value)) = rest.split_once('=') {
                let key = key.trim();
                let value = value.trim().trim_matches('"');
                if !key.is_empty() && !value.is_empty() && std::env::var(key).is_err() {
                    std::env::set_var(key, value);
                }
            }
        }
    }
}

/// Run the interactive onboarding. Returns the provider id to use.
/// This blocks on stdin — only call when a TTY is attached.
pub fn run_onboarding() -> anyhow::Result<String> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    writeln!(stdout)?;
    writeln!(stdout, "  ╭─────────────────────────────────────────────╮")?;
    writeln!(stdout, "  │   ✦ modron — first-run setup                │")?;
    writeln!(stdout, "  ╰─────────────────────────────────────────────╯")?;
    writeln!(stdout)?;
    writeln!(stdout, "  No AI provider is configured yet.")?;
    writeln!(stdout, "  Pick one to get started (you can change later):")?;
    writeln!(stdout)?;

    let options = provider_options();
    for opt in &options {
        writeln!(stdout, "   {} {:<20} {}", opt.number, opt.name, opt.note)?;
    }
    writeln!(stdout)?;
    writeln!(stdout, "   m  Try mock mode          (no key needed)")?;
    writeln!(stdout, "   q  Quit")?;
    writeln!(stdout)?;
    write!(stdout, "  choice: ")?;
    stdout.flush()?;

    let mut line = String::new();
    stdin.read_line(&mut line)?;
    let choice = line.trim();

    if choice.eq_ignore_ascii_case("q") {
        std::process::exit(0);
    }
    if choice.eq_ignore_ascii_case("m") {
        writeln!(stdout, "\n  starting in mock mode — no key needed\n")?;
        return Ok("mock".to_string());
    }

    let num: u8 = choice
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid choice: {choice}"))?;
    let opt = options
        .iter()
        .find(|o| o.number == num)
        .ok_or_else(|| anyhow::anyhow!("no provider {num}"))?;

    writeln!(stdout)?;
    writeln!(stdout, "  ── {} ──", opt.name)?;
    writeln!(stdout)?;
    writeln!(stdout, "  1. Get a key at: {}", opt.key_url)?;
    writeln!(stdout, "     (opens in your browser if you press Enter)")?;
    write!(stdout, "  press Enter to open, or type the key now: ")?;
    stdout.flush()?;

    let mut key_line = String::new();
    stdin.read_line(&mut key_line)?;
    let key = key_line.trim().to_string();

    if key.is_empty() {
        // Open the browser and prompt again.
        let _ = std::process::Command::new("open")
            .arg(opt.key_url)
            .spawn()
            .or_else(|_| {
                std::process::Command::new("xdg-open")
                    .arg(opt.key_url)
                    .spawn()
            });
        writeln!(stdout, "  opened {} — paste your key here:", opt.key_url)?;
        write!(stdout, "  key: ")?;
        stdout.flush()?;
        let mut key2 = String::new();
        stdin.read_line(&mut key2)?;
        if key2.trim().is_empty() {
            anyhow::bail!("no key provided — set {} and rerun", opt.env_var);
        }
        return save_and_confirm(opt, key2.trim());
    }

    save_and_confirm(opt, &key)
}

fn save_and_confirm(opt: &ProviderOption, key: &str) -> anyhow::Result<String> {
    // Save to ~/.config/modron/env (0600).
    let path = env_file()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Append (or replace the existing line for this var).
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<String> = existing
        .lines()
        .filter(|l| !l.starts_with(&format!("export {}=", opt.env_var)))
        .map(String::from)
        .collect();
    lines.push(format!("export {}=\"{}\"", opt.env_var, key));
    std::fs::write(&path, lines.join("\n") + "\n")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }

    // Set in the current process too.
    std::env::set_var(opt.env_var, key);

    let provider_id = opt.provider_id;

    // Save the default provider to config.toml.
    let config_path = dirs::home_dir()
        .map(|h| h.join(".config").join("modron").join("config.toml"))
        .ok_or_else(|| anyhow::anyhow!("no home"))?;
    let existing_config = std::fs::read_to_string(&config_path).unwrap_or_default();
    // Replace or add the default_provider line.
    let mut config_lines: Vec<String> = existing_config
        .lines()
        .filter(|l| !l.trim_start_matches('#').starts_with("default_provider"))
        .map(String::from)
        .collect();
    config_lines.insert(0, format!("default_provider = \"{provider_id}\""));
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&config_path, config_lines.join("\n") + "\n")?;

    writeln!(std::io::stdout())?;
    writeln!(
        std::io::stdout(),
        "  ✓ key saved to {} (mode 0600)",
        path.display()
    )?;
    writeln!(std::io::stdout(), "  ✓ default provider: {provider_id}\n")?;
    Ok(provider_id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_options_are_complete() {
        let opts = provider_options();
        assert!(opts.len() >= 8);
        assert!(opts.iter().all(|o| !o.env_var.is_empty()));
        assert!(opts.iter().all(|o| !o.key_url.is_empty()));
    }

    #[test]
    fn env_file_is_under_config_dir() {
        let path = env_file().unwrap();
        assert!(path.to_string_lossy().contains("modron"));
        assert!(path.to_string_lossy().contains("env"));
    }
}
