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
/// `modron auth` wizard, and the TUI `/auth` manager. Subscription
/// providers (Anthropic, OpenAI/ChatGPT) lead: they offer browser
/// sign-in, everything below them is key-only.
pub fn provider_registry() -> Vec<ProviderInfo> {
    vec![
        ProviderInfo {
            name: "Anthropic (Claude)",
            provider_id: "anthropic",
            env_var: "ANTHROPIC_API_KEY",
            key_url: "https://console.anthropic.com",
            note: "API key or Claude subscription (browser)",
        },
        ProviderInfo {
            name: "OpenAI / ChatGPT",
            provider_id: "openai-compat",
            env_var: "OPENAI_API_KEY",
            key_url: "https://platform.openai.com/api-keys",
            note: "API key or ChatGPT sign-in (browser)",
        },
        ProviderInfo {
            name: "Z.ai (GLM)",
            provider_id: "zai",
            env_var: "ZAI_API_KEY",
            key_url: "https://z.ai/manage-apikey/apikey-list",
            note: "free tier available (glm-4.5-flash)",
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

/// Whether a provider offers the in-app browser sign-in (OAuth) in
/// addition to API keys.
pub fn oauth_capable(provider_id: &str) -> bool {
    matches!(provider_id, "anthropic" | "openai-compat" | "zai")
}

/// Whether stored sign-in tokens exist for an OAuth-capable provider
/// (`~/.modron/auth/<provider>.json`).
pub fn oauth_signed_in(provider_id: &str) -> bool {
    let file = match provider_id {
        "anthropic" => oauth::auth_file(),
        "openai-compat" => oauth::chatgpt::token_file(),
        _ => return false,
    };
    file.map(|p| p.exists()).unwrap_or(false)
}

/// Whether a CATALOG provider id has usable credentials: an API key
/// (env or saved env file) or a stored OAuth sign-in. "chatgpt" rides
/// the OpenAI sign-in tokens; "openai"/"openai-compat" share
/// OPENAI_API_KEY.
pub fn catalog_provider_authenticated(provider_id: &str) -> bool {
    match provider_id {
        "chatgpt" => oauth_signed_in("openai-compat"),
        "anthropic" => {
            key_source("ANTHROPIC_API_KEY").0 != KeySource::None || oauth_signed_in("anthropic")
        }
        other => provider_info(other)
            .map(|info| key_source(info.env_var).0 != KeySource::None)
            .unwrap_or(false),
    }
}

/// Open `url` in the user's browser. Best-effort: returns false when no
/// opener exists (callers still render the URL as the manual fallback).
pub fn open_browser(url: &str) -> bool {
    #[cfg(target_os = "macos")]
    const OPENER: &str = "open";
    #[cfg(not(target_os = "macos"))]
    const OPENER: &str = "xdg-open";
    std::process::Command::new(OPENER).arg(url).spawn().is_ok()
}

/// The env file where keys are saved.
pub fn env_file() -> std::io::Result<std::path::PathBuf> {
    if let Ok(dir) = std::env::var("MODRON_CONFIG") {
        // Same convention as `modron-cli::config::config_path`: the
        // variable names the config DIRECTORY. An empty value means unset.
        if !dir.is_empty() {
            return Ok(std::path::PathBuf::from(dir).join("env"));
        }
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

/// True for a shell-style variable name: `[A-Za-z_][A-Za-z0-9_]*`.
fn valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// Refuse an entry the env file cannot hold safely. The value is written
/// inside double quotes and the file is line-oriented and sourced by the
/// shell, so newlines, quotes, backticks, `$` and backslashes would break
/// the line or inject commands. Errors never echo the value (it is a key).
fn validate_env_entry(env_var: &str, value: &str) -> std::io::Result<()> {
    use std::io::{Error, ErrorKind};
    if !valid_env_name(env_var) {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!(
                "invalid environment variable name {env_var:?}: use letters, digits and \
                 underscores, and do not start with a digit"
            ),
        ));
    }
    if let Some(bad) = value
        .chars()
        .find(|c| matches!(c, '\n' | '\r' | '"' | '`' | '$' | '\\'))
    {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!(
                "refusing to store {env_var}: the value contains the forbidden character {bad:?} \
                 (newlines, quotes, backticks, '$' and backslashes are not allowed in keys)"
            ),
        ));
    }
    Ok(())
}

/// Write/replace one provider key in the env file (0600 on create). The
/// name and value are validated BEFORE the file is touched, and the write
/// is atomic (see [`write_private`]).
pub fn set_key_in_env_file(
    path: &std::path::Path,
    env_var: &str,
    key: &str,
) -> std::io::Result<()> {
    validate_env_entry(env_var, key)?;
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
    write_private(path, &(lines.join("\n") + "\n"))
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
    write_private(path, &(lines.join("\n") + "\n"))?;
    Ok(true)
}

/// Replace a file holding credentials ATOMICALLY and privately: the new
/// contents go to a uniquely named temp file in the same directory, created
/// 0600 from the start (`create_new`, so a stale or planted name is never
/// reused), synced, then renamed over the target. A crash or a concurrent
/// reader sees either the old file or the new one, never a torn write, and
/// a pre-existing wider mode is replaced rather than kept.
pub fn write_private(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    use std::io::{Error, ErrorKind};
    let dir = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => std::path::Path::new("."),
    };
    let file_name = path
        .file_name()
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "credential path has no file name"))?;
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut collisions = 0;
    let (tmp_path, file) = loop {
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mut tmp_name = std::ffi::OsString::from(".");
        tmp_name.push(file_name);
        tmp_name.push(format!(".{}.{nanos}.{seq}.tmp", std::process::id()));
        let tmp_path = dir.join(tmp_name);
        match create_private_new(&tmp_path) {
            Ok(file) => break (tmp_path, file),
            Err(err) if err.kind() == ErrorKind::AlreadyExists && collisions < 8 => {
                collisions += 1;
            }
            Err(err) => return Err(err),
        }
    };
    let result = commit_private(file, &tmp_path, path, contents);
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    result
}

fn create_private_new(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)
}

fn commit_private(
    mut file: std::fs::File,
    tmp_path: &std::path::Path,
    path: &std::path::Path,
    contents: &str,
) -> std::io::Result<()> {
    use std::io::Write as _;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(tmp_path, path)
}

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

/// ── Existing sign-in discovery ───────────────────────────────────
///
/// Other agent CLIs on this machine already hold subscription sign-ins
/// that Modron speaks natively (Codex CLI's ChatGPT tokens; Claude
/// Code's Anthropic tokens). Onboarding scans these stores and offers
/// one-key reuse instead of a fresh browser round-trip.
pub mod discovery {
    use super::oauth::{self, chatgpt};
    use serde_json::Value;
    use std::path::Path;

    /// A sign-in stored by another tool that Modron can import.
    #[derive(Clone, Debug)]
    pub struct DetectedSignin {
        /// Modron provider id the imported tokens unlock
        /// ("chatgpt" | "anthropic").
        pub provider_id: &'static str,
        /// Human label of the source store (e.g. "Codex CLI
        /// (~/.codex/auth.json)").
        pub source: String,
        pub credentials: DetectedCredentials,
    }

    /// The tokens to persist on import — the two subscription flows
    /// Modron implements end to end.
    #[derive(Clone, PartialEq, Eq)]
    pub enum DetectedCredentials {
        ChatGpt {
            access_token: String,
            refresh_token: Option<String>,
            account_id: String,
        },
        Anthropic {
            access_token: String,
            refresh_token: Option<String>,
        },
    }

    impl std::fmt::Debug for DetectedCredentials {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::ChatGpt {
                    refresh_token,
                    account_id,
                    ..
                } => f
                    .debug_struct("ChatGpt")
                    .field("access_token", &"<redacted>")
                    .field("refresh_token", &oauth::redacted_opt(refresh_token))
                    .field("account_id", account_id)
                    .finish(),
                Self::Anthropic { refresh_token, .. } => f
                    .debug_struct("Anthropic")
                    .field("access_token", &"<redacted>")
                    .field("refresh_token", &oauth::redacted_opt(refresh_token))
                    .finish(),
            }
        }
    }

    impl DetectedSignin {
        /// Friendly provider name for pickers and confirmations.
        pub fn provider_label(&self) -> &'static str {
            match self.provider_id {
                "chatgpt" => "ChatGPT",
                _ => "Claude (Anthropic)",
            }
        }

        /// Masked access token for confirmation lines (never the value).
        pub fn token_mask(&self) -> String {
            let token = match &self.credentials {
                DetectedCredentials::ChatGpt { access_token, .. }
                | DetectedCredentials::Anthropic { access_token, .. } => access_token,
            };
            super::mask_key(token)
        }
    }

    /// Scan the well-known credential stores under `home` (tests inject a
    /// tempdir). Sign-ins Modron already stores itself are skipped — a
    /// stored sign-in is "signed in", not "importable".
    pub fn find_existing_signins_in(home: &Path) -> Vec<DetectedSignin> {
        find_existing_signins_with(home, claude_code_keychain)
    }

    /// Scan under $HOME — the production entry point.
    pub fn find_existing_signins() -> Vec<DetectedSignin> {
        let home = std::env::var("HOME").unwrap_or_default();
        find_existing_signins_in(Path::new(&home))
    }

    fn find_existing_signins_with(
        home: &Path,
        keychain_probe: fn() -> Option<String>,
    ) -> Vec<DetectedSignin> {
        let mut found = Vec::new();

        // Codex CLI (ChatGPT subscription): ~/.codex/auth.json, tokens
        // nested under "tokens" (older builds stored them flat).
        if !modron_chatgpt_stored_in(home) {
            if let Some(text) = read_file(&home.join(".codex").join("auth.json")) {
                if let Some(credentials) = parse_codex_auth(&text) {
                    found.push(DetectedSignin {
                        provider_id: "chatgpt",
                        source: "Codex CLI (~/.codex/auth.json)".into(),
                        credentials,
                    });
                }
            }
        }

        // Claude Code: ~/.claude/.credentials.json, else the macOS
        // keychain item (probe injectable — tests must stay hermetic).
        // Anthropic refresh tokens ROTATE — imported copies go stale
        // when either side refreshes (re-import fixes).
        if !modron_anthropic_stored_in(home) {
            let file = home.join(".claude").join(".credentials.json");
            if let Some(text) = read_file(&file) {
                if let Some(credentials) = parse_claude_credentials(&text) {
                    found.push(DetectedSignin {
                        provider_id: "anthropic",
                        source: "Claude Code (~/.claude/.credentials.json)".into(),
                        credentials,
                    });
                }
            } else if let Some(text) = keychain_probe() {
                if let Some(credentials) = parse_claude_credentials(&text) {
                    found.push(DetectedSignin {
                        provider_id: "anthropic",
                        source: "Claude Code (macOS Keychain)".into(),
                        credentials,
                    });
                }
            }
        }

        found
    }

    /// Import: persist the tokens into Modron's own 0600 token store.
    /// The source tool's copy is untouched.
    pub fn import_signin(signin: &DetectedSignin) -> anyhow::Result<()> {
        match &signin.credentials {
            DetectedCredentials::ChatGpt {
                access_token,
                refresh_token,
                account_id,
            } => chatgpt::save(&chatgpt::ChatGptAuth {
                access_token: access_token.clone(),
                refresh_token: refresh_token.clone(),
                account_id: account_id.clone(),
            }),
            DetectedCredentials::Anthropic {
                access_token,
                refresh_token,
            } => oauth::save_tokens(&oauth::TokenSet {
                access_token: access_token.clone(),
                refresh_token: refresh_token.clone(),
                expires_in: None,
                id_token: None,
            }),
        }
    }

    /// Parse Codex CLI's auth.json. Requires a non-empty access token
    /// and a derivable account id (explicit field, else the id_token's
    /// `https://api.openai.com/auth` claim).
    pub fn parse_codex_auth(text: &str) -> Option<DetectedCredentials> {
        let value: Value = serde_json::from_str(text).ok()?;
        // Nested "tokens" object (2026 layout); fall back to flat keys.
        let tokens = value.get("tokens").unwrap_or(&value);
        let access_token = tokens.get("access_token")?.as_str()?;
        if access_token.is_empty() {
            return None;
        }
        let refresh_token = non_empty(tokens.get("refresh_token"));
        let account_id = non_empty(tokens.get("account_id")).or_else(|| {
            tokens
                .get("id_token")
                .and_then(Value::as_str)
                .and_then(chatgpt::account_id)
        })?;
        Some(DetectedCredentials::ChatGpt {
            access_token: access_token.to_string(),
            refresh_token,
            account_id,
        })
    }

    /// Parse Claude Code's credentials JSON (file or keychain payload):
    /// `{"claudeAiOauth": {"accessToken": …, "refreshToken": …}}`
    /// (flat access_token/access_token-style keys accepted too).
    pub fn parse_claude_credentials(text: &str) -> Option<DetectedCredentials> {
        let value: Value = serde_json::from_str(text).ok()?;
        let oauth = value.get("claudeAiOauth").unwrap_or(&value);
        let access_token = oauth
            .get("accessToken")
            .or_else(|| oauth.get("access_token"))?
            .as_str()?;
        if access_token.is_empty() {
            return None;
        }
        let refresh_token = oauth
            .get("refreshToken")
            .or_else(|| oauth.get("refresh_token"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);
        Some(DetectedCredentials::Anthropic {
            access_token: access_token.to_string(),
            refresh_token,
        })
    }

    fn non_empty(value: Option<&Value>) -> Option<String> {
        value
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
    }

    fn read_file(path: &Path) -> Option<String> {
        std::fs::read_to_string(path)
            .ok()
            .filter(|t| !t.trim().is_empty())
    }

    fn modron_chatgpt_stored_in(home: &Path) -> bool {
        home.join(".modron")
            .join("auth")
            .join("openai.json")
            .exists()
    }

    fn modron_anthropic_stored_in(home: &Path) -> bool {
        home.join(".modron")
            .join("auth")
            .join("anthropic.json")
            .exists()
    }

    /// Claude Code stores its OAuth blob in the login keychain on macOS
    /// (service "Claude Code-credentials"). Reading the user's own item
    /// needs no prompt; a denied/locked keychain is a plain None.
    #[cfg(target_os = "macos")]
    fn claude_code_keychain() -> Option<String> {
        let output = std::process::Command::new("security")
            .args([
                "find-generic-password",
                "-s",
                "Claude Code-credentials",
                "-w",
            ])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        (!text.is_empty()).then_some(text)
    }
    #[cfg(not(target_os = "macos"))]
    fn claude_code_keychain() -> Option<String> {
        None
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn codex_fixture(account: Option<&str>) -> String {
            let mut tokens = serde_json::json!({
                "access_token": "at-codex",
                "refresh_token": "rt-codex",
                "id_token": "ignored.jwt.here",
            });
            if let Some(account) = account {
                tokens["account_id"] = serde_json::json!(account);
            }
            serde_json::json!({ "auth_mode": "chatgpt", "tokens": tokens }).to_string()
        }

        #[test]
        fn codex_nested_tokens_with_account_field() {
            let creds = parse_codex_auth(&codex_fixture(Some("acc-1"))).expect("parses");
            assert_eq!(
                creds,
                DetectedCredentials::ChatGpt {
                    access_token: "at-codex".into(),
                    refresh_token: Some("rt-codex".into()),
                    account_id: "acc-1".into(),
                }
            );
        }

        #[test]
        fn codex_account_from_id_token_claim_when_field_missing() {
            // JWT payload with the nested account claim (the LIVE-verified
            // claim path from the ChatGPT sign-in work).
            let payload = super::super::oauth::base64url_nopad(
                br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acc-claim"}}"#,
            );
            let id_token = format!("h.{payload}.s");
            let text = serde_json::json!({
                "tokens": {
                    "access_token": "at-2",
                    "refresh_token": "rt-2",
                    "id_token": id_token,
                }
            })
            .to_string();
            let creds = parse_codex_auth(&text).expect("parses");
            match creds {
                DetectedCredentials::ChatGpt { account_id, .. } => {
                    assert_eq!(account_id, "acc-claim");
                }
                other => panic!("wrong variant: {other:?}"),
            }
        }

        #[test]
        fn codex_without_access_token_or_account_is_rejected() {
            assert!(parse_codex_auth("{}").is_none());
            assert!(
                parse_codex_auth(r#"{"tokens":{"access_token":"","refresh_token":"rt"}}"#)
                    .is_none()
            );
            // Access token present but no account derivable.
            assert!(parse_codex_auth(r#"{"tokens":{"access_token":"at"}}"#).is_none());
        }

        #[test]
        fn claude_credentials_shapes() {
            let creds = parse_claude_credentials(
                r#"{"claudeAiOauth":{"accessToken":"at-claude","refreshToken":"rt-claude","expiresAt":"2026"}}"#,
            )
            .expect("parses");
            assert_eq!(
                creds,
                DetectedCredentials::Anthropic {
                    access_token: "at-claude".into(),
                    refresh_token: Some("rt-claude".into()),
                }
            );
            // Keychain payloads can omit the refresh token.
            let creds = parse_claude_credentials(r#"{"claudeAiOauth":{"accessToken":"at-only"}}"#)
                .expect("parses");
            assert!(
                matches!(
                    creds,
                    DetectedCredentials::Anthropic {
                        refresh_token: None,
                        ..
                    }
                ),
                "{creds:?}"
            );
            assert!(parse_claude_credentials("not json").is_none());
        }

        #[test]
        fn finds_codex_and_claude_files_in_a_fake_home() {
            let home = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(home.path().join(".codex")).unwrap();
            std::fs::write(
                home.path().join(".codex").join("auth.json"),
                codex_fixture(Some("acc-9")),
            )
            .unwrap();
            std::fs::create_dir_all(home.path().join(".claude")).unwrap();
            std::fs::write(
                home.path().join(".claude").join(".credentials.json"),
                r#"{"claudeAiOauth":{"accessToken":"at-c","refreshToken":"rt-c"}}"#,
            )
            .unwrap();

            let found = find_existing_signins_with(home.path(), || None);
            let ids: Vec<&str> = found.iter().map(|s| s.provider_id).collect();
            assert_eq!(ids, vec!["chatgpt", "anthropic"], "{found:?}");
            assert!(found[0].source.contains("Codex"));
            assert_eq!(found[0].provider_label(), "ChatGPT");
            assert_eq!(found[1].provider_label(), "Claude (Anthropic)");
        }

        #[test]
        fn keychain_probe_supplies_the_claude_signin() {
            let home = tempfile::tempdir().unwrap();
            let found = find_existing_signins_with(home.path(), || {
                Some(r#"{"claudeAiOauth":{"accessToken":"at-kc","refreshToken":"rt-kc"}}"#.into())
            });
            assert_eq!(found.len(), 1, "{found:?}");
            assert_eq!(found[0].provider_id, "anthropic");
            assert!(found[0].source.contains("Keychain"), "{found:?}");
        }

        #[test]
        fn skips_stores_modron_already_owns() {
            let home = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(home.path().join(".codex")).unwrap();
            std::fs::write(
                home.path().join(".codex").join("auth.json"),
                codex_fixture(Some("acc-9")),
            )
            .unwrap();
            // Modron already has a ChatGPT sign-in: the codex store is not
            // importable, and nothing else is present.
            std::fs::create_dir_all(home.path().join(".modron").join("auth")).unwrap();
            std::fs::write(
                home.path().join(".modron").join("auth").join("openai.json"),
                "{}",
            )
            .unwrap();
            assert!(find_existing_signins_with(home.path(), || None).is_empty());
        }
    }
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
    fn catalog_provider_auth_maps_keys_and_signins() {
        // chatgpt rides the OpenAI sign-in tokens, not a key.
        assert_eq!(
            catalog_provider_authenticated("chatgpt"),
            oauth_signed_in("openai-compat") || false
        );
        // anthropic accepts either path.
        assert_eq!(
            catalog_provider_authenticated("anthropic"),
            key_source("ANTHROPIC_API_KEY").0 != KeySource::None || oauth_signed_in("anthropic")
        );
        // Registry providers key off their env var; unknown ids are out.
        let zai = provider_info("zai").unwrap();
        assert_eq!(
            catalog_provider_authenticated("zai"),
            key_source(zai.env_var).0 != KeySource::None
        );
        assert!(!catalog_provider_authenticated("nope"));
    }

    #[test]
    fn subscription_providers_lead_the_registry() {
        let reg = provider_registry();
        assert_eq!(reg[0].provider_id, "anthropic");
        assert_eq!(reg[1].provider_id, "openai-compat");
        // Only those two offer browser sign-in.
        for info in &reg {
            assert_eq!(
                oauth_capable(info.provider_id),
                matches!(info.provider_id, "anthropic" | "openai-compat" | "zai"),
                "{}",
                info.provider_id
            );
        }
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
    fn env_file_rejects_unsafe_names_and_values_and_keeps_the_file() {
        let dir = std::env::temp_dir().join(format!("modron-core-envval-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("env");
        set_key_in_env_file(&path, "ZAI_API_KEY", "sk-good").unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        let bad_values = [
            "a\nexport EVIL=\"1",
            "a\rb",
            "a\"b",
            "$(id)",
            "a`id`",
            "a$HOME",
            "a\\b",
        ];
        for value in bad_values {
            let err = set_key_in_env_file(&path, "GROQ_API_KEY", value)
                .expect_err("unsafe value must be rejected");
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{err}");
        }
        let bad_names = ["BAD\nNAME", "A\"B", "1ABC", "A-B", "", "A B", "$(id)"];
        for name in bad_names {
            assert!(
                set_key_in_env_file(&path, name, "sk-ok").is_err(),
                "name {name:?} must be rejected"
            );
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_file_write_is_atomic_private_and_leaves_no_temp_files() {
        let dir = std::env::temp_dir().join(format!("modron-core-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("env");
        set_key_in_env_file(&path, "ZAI_API_KEY", "sk-round_trip.1").unwrap();
        let pairs = parse_env_file(&std::fs::read_to_string(&path).unwrap());
        assert_eq!(
            pairs,
            vec![("ZAI_API_KEY".to_string(), "sk-round_trip.1".to_string())]
        );
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["env".to_string()], "temp file left behind");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn credential_structs_debug_without_secrets() {
        let creds = discovery::DetectedCredentials::ChatGpt {
            access_token: "SECRET-CG-AT".into(),
            refresh_token: Some("SECRET-CG-RT".into()),
            account_id: "acct-visible".into(),
        };
        let dbg = format!("{creds:?}");
        assert!(!dbg.contains("SECRET-CG"), "{dbg}");
        assert!(dbg.contains("acct-visible"), "{dbg}");
        let anth = discovery::DetectedCredentials::Anthropic {
            access_token: "SECRET-AN-AT".into(),
            refresh_token: Some("SECRET-AN-RT".into()),
        };
        assert!(!format!("{anth:?}").contains("SECRET-AN"));
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

// ── OAuth (Anthropic subscription, PKCE + localhost callback) ─────────────
//
// Moved from modron-cli so the TUI can run the same flow interactively.
// Flow (RFC 7636): prepare_login() builds the authorize URL (pure);
// wait_for_callback() listens once on localhost; exchange_code() trades
// the code for tokens at the documented endpoint. STATUS: unverified
// against the live endpoint (needs a subscription + browser round-trip).
// ToS-gray: API keys remain the first-class path.

pub mod oauth {
    use anyhow::anyhow;
    use serde_json::{json, Value};
    use sha2::{Digest, Sha256};
    use std::path::PathBuf;

    /// Bounded HTTP client for every OAuth exchange/refresh call: a stalled
    /// token endpoint must fail with a timeout instead of hanging a sign-in
    /// (or the 15-minute device-flow deadline) forever.
    fn oauth_client() -> anyhow::Result<reqwest::Client> {
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|e| anyhow!("cannot build http client: {e}"))
    }

    /// Claude Code's public OAuth client id — the documented,
    /// widely-integrated id for subscription flows.
    pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
    pub const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
    pub const TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
    pub const SCOPES: &str = "org:create_api_key user:profile user:inference";
    /// The localhost callback port (one-shot listener).
    pub const CALLBACK_PORT: u16 = 54545;

    /// A prepared login: everything needed to show the URL and later
    /// exchange the code (the verifier must stay secret until then).
    #[derive(Clone, PartialEq, Eq)]
    pub struct LoginSession {
        pub url: String,
        pub state: String,
        pub verifier: String,
        pub port: u16,
    }

    impl std::fmt::Debug for LoginSession {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            // `url` embeds the state, so it is redacted too.
            f.debug_struct("LoginSession")
                .field("url", &"<redacted>")
                .field("state", &"<redacted>")
                .field("verifier", &"<redacted>")
                .field("port", &self.port)
                .finish()
        }
    }

    /// Prepare a login: PKCE pair + state + the authorize URL, bound to a
    /// callback port PROBED for availability (54545..=54555). Concurrent
    /// modron sessions must not break each other's sign-in: a fixed port
    /// made a second session's login fail with address-in-use while the
    /// first was still waiting on its listener.
    pub fn prepare_login() -> anyhow::Result<LoginSession> {
        let (verifier, challenge) = pkce_pair(None)?;
        let state = base64url_nopad(&rand_bytes()?);
        let port = probe_callback_port();
        Ok(LoginSession {
            url: authorize_url(&challenge, &state, port),
            state,
            verifier,
            port,
        })
    }

    /// First bindable port in the callback range (falls back to the base
    /// port when all are taken — the listener bind then surfaces the
    /// address-in-use error, which is at least honest). The probe releases
    /// the socket; `wait_for_callback` binds it again moments later.
    fn probe_callback_port() -> u16 {
        for port in CALLBACK_PORT..CALLBACK_PORT + 40 {
            if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
                return port;
            }
        }
        CALLBACK_PORT
    }

    /// Wait for the one-shot OAuth callback on localhost. Returns the
    /// authorization code (state-validated); the socket answers the
    /// browser with a plain "return to your terminal" page. When
    /// `open_url` is given the browser is opened AFTER the listener
    /// binds, so an instant approval can never race the socket.
    pub async fn wait_for_callback(
        port: u16,
        expected_state: &str,
        open_url: Option<&str>,
    ) -> anyhow::Result<String> {
        let (v4, v6) = bind_callback_listeners(port).await?;
        if let Some(url) = open_url {
            super::open_browser(url);
        }
        await_callback(v4, v6, expected_state, CALLBACK_TIMEOUT).await
    }

    /// How long the browser has to complete sign-in before the listener
    /// gives up. Stray or invalid requests never end the wait early.
    pub const CALLBACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

    /// Bind the callback socket on IPv4 loopback (required) and IPv6
    /// loopback (best effort). A browser that resolves `localhost` to
    /// `::1` then reaches the same listener instead of getting refused.
    async fn bind_callback_listeners(
        port: u16,
    ) -> anyhow::Result<(tokio::net::TcpListener, Option<tokio::net::TcpListener>)> {
        let v4 = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
        let v6 = tokio::net::TcpListener::bind(("::1", port)).await.ok();
        Ok((v4, v6))
    }

    /// Accept connections until one carries the expected `state` (and a
    /// code, or an authorization error). Each connection is served on its
    /// own task so a stalled stray socket cannot hold up the real callback.
    async fn await_callback(
        v4: tokio::net::TcpListener,
        v6: Option<tokio::net::TcpListener>,
        expected_state: &str,
        timeout: std::time::Duration,
    ) -> anyhow::Result<String> {
        let (done_tx, mut done_rx) =
            tokio::sync::mpsc::unbounded_channel::<anyhow::Result<String>>();
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => {
                    return Err(anyhow!(
                        "sign-in timed out after {timeout:?} waiting for the browser callback — run it again"
                    ));
                }
                accepted = accept_either(&v4, v6.as_ref()) => {
                    let socket = accepted?;
                    let expected = expected_state.to_string();
                    let done = done_tx.clone();
                    tokio::spawn(serve_callback_request(socket, expected, done));
                }
                Some(result) = done_rx.recv() => return result,
            }
        }
    }

    async fn accept_either(
        v4: &tokio::net::TcpListener,
        v6: Option<&tokio::net::TcpListener>,
    ) -> std::io::Result<tokio::net::TcpStream> {
        match v6 {
            Some(v6) => tokio::select! {
                r = v4.accept() => r.map(|(socket, _)| socket),
                r = v6.accept() => r.map(|(socket, _)| socket),
            },
            None => v4.accept().await.map(|(socket, _)| socket),
        }
    }

    const SUCCESS_PAGE: &str = "Authorized. Return to your terminal.";
    const FAILURE_PAGE: &str = "Sign-in was not completed. Return to your terminal and try again.";

    /// Answer one callback connection. Only a request that validates
    /// (right path, matching state, a code or an authorization error) gets
    /// a verdict on the login; everything else is answered 400 and the
    /// listener keeps waiting.
    async fn serve_callback_request(
        mut socket: tokio::net::TcpStream,
        expected_state: String,
        done: tokio::sync::mpsc::UnboundedSender<anyhow::Result<String>>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // Read the request head: it may arrive TCP-segmented. Bounded in
        // both size and time so a silent probe connection is just dropped.
        let head = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let mut buf: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 4096];
            while buf.len() < 16384 && !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = socket.read(&mut chunk).await?;
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            Ok::<_, std::io::Error>(String::from_utf8_lossy(&buf).into_owned())
        })
        .await;
        let Ok(Ok(head)) = head else {
            return;
        };
        let verdict = classify_request_head(&head, &expected_state);
        let (status, page) = match &verdict {
            CallbackVerdict::Code(_) => ("200 OK", SUCCESS_PAGE),
            CallbackVerdict::Failed(_) | CallbackVerdict::Stray => {
                ("400 Bad Request", FAILURE_PAGE)
            }
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
            page.len()
        );
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.shutdown().await;
        match verdict {
            CallbackVerdict::Code(code) => {
                let _ = done.send(Ok(code));
            }
            CallbackVerdict::Failed(message) => {
                let _ = done.send(Err(anyhow!(message)));
            }
            CallbackVerdict::Stray => {}
        }
    }

    /// Outcome of looking at one callback request.
    #[derive(Debug, PartialEq, Eq)]
    enum CallbackVerdict {
        /// Validated: state matches and a code was delivered.
        Code(String),
        /// State matches but the authorization server reported a failure
        /// (or sent no code): the login ends with this message.
        Failed(String),
        /// Not our callback (wrong path, method or state, or unparsable):
        /// ignored, the listener keeps waiting.
        Stray,
    }

    /// Validate the request line and query of a callback request head.
    fn classify_request_head(head: &str, expected_state: &str) -> CallbackVerdict {
        // "GET /callback?code=…&state=… HTTP/1.1"
        let mut parts = head.lines().next().unwrap_or("").split_whitespace();
        let method = parts.next().unwrap_or("");
        let target = parts.next().unwrap_or("");
        if method != "GET" {
            return CallbackVerdict::Stray;
        }
        match target.split_once('?') {
            Some(("/callback", query)) => classify_query(query, expected_state),
            _ => CallbackVerdict::Stray,
        }
    }

    /// Validate a callback query string. Values are percent-decoded before
    /// they are compared or returned.
    fn classify_query(query: &str, expected_state: &str) -> CallbackVerdict {
        let mut code: Option<String> = None;
        let mut error: Option<String> = None;
        let mut saw_state = false;
        for pair in query.split('&') {
            let (raw_key, raw_value) = pair.split_once('=').unwrap_or((pair, ""));
            let (Some(key), Some(value)) = (percent_decode(raw_key), percent_decode(raw_value))
            else {
                return CallbackVerdict::Stray;
            };
            match key.as_str() {
                "state" => {
                    // Any state that differs (even alongside a valid one)
                    // makes the request suspect: not our callback.
                    if expected_state.is_empty() || value != expected_state {
                        return CallbackVerdict::Stray;
                    }
                    saw_state = true;
                }
                "code" => code = Some(value),
                "error" => error = Some(value),
                _ => {}
            }
        }
        if !saw_state {
            return CallbackVerdict::Stray;
        }
        if let Some(error) = error {
            return CallbackVerdict::Failed(format!("authorization failed: {error}"));
        }
        match code {
            Some(code) if !code.is_empty() => CallbackVerdict::Code(code),
            _ => CallbackVerdict::Failed("callback URL missing code".to_string()),
        }
    }

    /// Decode `%XX` escapes (`+` is left alone: authorization codes are
    /// URL-safe). None for a malformed escape or non-UTF-8 output.
    fn percent_decode(raw: &str) -> Option<String> {
        let bytes = raw.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' {
                let hex = bytes.get(i + 1..i + 3)?;
                if !hex.iter().all(u8::is_ascii_hexdigit) {
                    return None;
                }
                let text = std::str::from_utf8(hex).ok()?;
                out.push(u8::from_str_radix(text, 16).ok()?);
                i += 3;
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        String::from_utf8(out).ok()
    }

    /// Exchange the callback code for tokens. Anthropic's endpoint is
    /// CUSTOM JSON (not the RFC form encoding) and requires `state` in
    /// the body — learned live: form encoding or a missing state both
    /// answer "Invalid request format".
    pub async fn exchange_code(session: &LoginSession, code: &str) -> anyhow::Result<TokenSet> {
        exchange_code_at(TOKEN_URL, session, code).await
    }

    /// Endpoint-parameterized [`exchange_code`] (tests point it at a
    /// mock and assert the wire format).
    pub async fn exchange_code_at(
        token_url: &str,
        session: &LoginSession,
        code: &str,
    ) -> anyhow::Result<TokenSet> {
        let client = oauth_client()?;
        let response = client
            .post(token_url)
            .json(&token_exchange_body(
                code,
                &session.state,
                &session.verifier,
                session.port,
            ))
            .send()
            .await?;
        // No error_for_status: this endpoint answers 200 with an error
        // JSON body, and the body is the diagnostic worth surfacing. The
        // status is still folded into any failure so a non-JSON error body
        // (proxy 502 HTML) doesn't surface as an opaque decode error.
        let (status, text) = read_token_response(response).await?;
        let body: Value = serde_json::from_str(&text)
            .map_err(|e| anyhow!("HTTP {status}: malformed token response: {e}"))?;
        parse_token_response(&body).map_err(|e| match status.is_success() {
            true => e,
            false => anyhow!("HTTP {status}: {e:#}"),
        })
    }

    /// The JSON body of the authorization_code exchange (mirrors Claude
    /// Code's reverse-engineered request — state included).
    pub fn token_exchange_body(code: &str, state: &str, verifier: &str, port: u16) -> Value {
        json!({
            "grant_type": "authorization_code",
            "code": code,
            "state": state,
            "client_id": CLIENT_ID,
            "redirect_uri": format!("http://localhost:{port}/callback"),
            "code_verifier": verifier,
        })
    }

    /// Refresh a Claude-subscription access token. Same custom-JSON
    /// endpoint; refresh tokens ROTATE — the response carries a NEW
    /// refresh_token and the old one dies, so callers must persist the
    /// returned pair.
    /// Endpoint-parameterized refresh (tests point it at a
    /// mock and assert the wire format).
    pub async fn refresh_tokens_at(
        token_url: &str,
        refresh_token: &str,
    ) -> anyhow::Result<TokenSet> {
        let client = oauth_client()?;
        let response = client
            .post(token_url)
            .json(&json!({
                "grant_type": "refresh_token",
                "refresh_token": refresh_token,
                "client_id": CLIENT_ID,
            }))
            .send()
            .await?;
        let (status, text) = read_token_response(response).await?;
        let body: Value = serde_json::from_str(&text)
            .map_err(|e| anyhow!("HTTP {status}: malformed token response: {e}"))?;
        parse_token_response(&body).map_err(|e| match status.is_success() {
            true => e,
            false => anyhow!("HTTP {status}: {e:#}"),
        })
    }

    /// Read a token-endpoint response as (status, body text): the status
    /// must survive into error messages — a proxy's HTML 502 would
    /// otherwise surface as an opaque body-decode failure.
    async fn read_token_response(
        response: reqwest::Response,
    ) -> anyhow::Result<(reqwest::StatusCode, String)> {
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| anyhow!("HTTP {status}: cannot read token response: {e}"))?;
        Ok((status, text))
    }

    /// PKCE pair: (verifier, S256 challenge). Verifier is 64 chars from
    /// the unreserved set; challenge is base64url(SHA256(verifier)) without
    /// padding.
    pub fn pkce_pair(verifier_seed: Option<&str>) -> anyhow::Result<(String, String)> {
        let verifier = match verifier_seed {
            Some(seed) => seed.to_string(),
            None => {
                // 48 random bytes -> 64 base64url chars (OS randomness).
                let bytes: [u8; 48] = rand_bytes()?;
                base64url_nopad(&bytes).chars().take(64).collect()
            }
        };
        if !(43..=128).contains(&verifier.len()) {
            return Err(anyhow!(
                "PKCE verifier must be 43-128 chars, got {}",
                verifier.len()
            ));
        }
        let digest = Sha256::digest(verifier.as_bytes());
        Ok((verifier, base64url_nopad(&digest)))
    }

    pub fn base64url_nopad(data: &[u8]) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
        for chunk in data.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = *chunk.get(1).unwrap_or(&0) as u32;
            let b2 = *chunk.get(2).unwrap_or(&0) as u32;
            let n = (b0 << 16) | (b1 << 8) | b2;
            out.push(TABLE[(n >> 18) as usize & 63] as char);
            out.push(TABLE[(n >> 12) as usize & 63] as char);
            if chunk.len() > 1 {
                out.push(TABLE[(n >> 6) as usize & 63] as char);
            }
            if chunk.len() > 2 {
                out.push(TABLE[n as usize & 63] as char);
            }
        }
        out
    }

    /// OS randomness without new dependencies: /dev/urandom on unix.
    fn rand_bytes() -> anyhow::Result<[u8; 48]> {
        use std::io::Read;
        let mut buf = [0u8; 48];
        let mut f = std::fs::File::open("/dev/urandom")
            .map_err(|e| anyhow!("cannot read /dev/urandom: {e}"))?;
        f.read_exact(&mut buf)?;
        Ok(buf)
    }

    /// Build the authorize URL the user opens in a browser.
    pub fn authorize_url(challenge: &str, state: &str, port: u16) -> String {
        format!(
            "{AUTHORIZE_URL}?client_id={CLIENT_ID}&response_type=code&redirect_uri=http://localhost:{port}/callback&scope={}&code_challenge={challenge}&code_challenge_method=S256&state={state}",
            SCOPES.replace(' ', "%20")
        )
    }

    /// Parse a token endpoint response. Refresh tokens may be absent on
    /// refresh grants that reuse the existing one.
    #[derive(Clone, PartialEq, Eq)]
    pub struct TokenSet {
        pub access_token: String,
        pub refresh_token: Option<String>,
        pub expires_in: Option<u64>,
        /// OIDC id token when the flow returns one (OpenAI carries the
        /// chatgpt_account_id claim here).
        pub id_token: Option<String>,
    }

    impl std::fmt::Debug for TokenSet {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("TokenSet")
                .field("access_token", &"<redacted>")
                .field("refresh_token", &redacted_opt(&self.refresh_token))
                .field("expires_in", &self.expires_in)
                .field("id_token", &redacted_opt(&self.id_token))
                .finish()
        }
    }

    /// Debug helper for secret fields: shows whether a secret is present
    /// (`Some("<redacted>")` / `None`) without ever printing its value.
    pub(super) fn redacted_opt(value: &Option<String>) -> Option<&'static str> {
        value.as_ref().map(|_| "<redacted>")
    }

    /// Describe a response body for an error message WITHOUT echoing it:
    /// the body may hold tokens. Only top-level field names are reported
    /// (or a note that the body is not a JSON object).
    pub(super) fn describe_body(body: &Value) -> String {
        match body {
            Value::Object(map) => {
                let mut names: Vec<&str> = map.keys().map(String::as_str).collect();
                names.sort_unstable();
                names.truncate(16);
                format!("response fields: [{}]", names.join(", "))
            }
            _ => "response is not a JSON object".to_string(),
        }
    }

    /// [`describe_body`] for raw response text: fields when it is JSON,
    /// otherwise only its byte length.
    pub(super) fn describe_text(text: &str) -> String {
        match serde_json::from_str::<Value>(text) {
            Ok(value) => describe_body(&value),
            Err(_) => format!("non-JSON response of {} bytes", text.len()),
        }
    }

    pub fn parse_token_response(body: &Value) -> anyhow::Result<TokenSet> {
        let access_token = body
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                anyhow!(
                    "token response missing access_token ({})",
                    describe_body(body)
                )
            })?
            .to_string();
        Ok(TokenSet {
            access_token,
            refresh_token: body
                .get("refresh_token")
                .and_then(|v| v.as_str())
                .map(String::from),
            expires_in: body.get("expires_in").and_then(|v| v.as_u64()),
            id_token: body
                .get("id_token")
                .and_then(|v| v.as_str())
                .map(String::from),
        })
    }

    /// Token storage: `~/.modron/auth/anthropic.json`, 0600.
    pub fn auth_file() -> anyhow::Result<PathBuf> {
        let home = std::env::var("HOME").map_err(|_| anyhow!("no home directory"))?;
        Ok(std::path::PathBuf::from(home)
            .join(".modron")
            .join("auth")
            .join("anthropic.json"))
    }

    pub fn save_tokens(tokens: &TokenSet) -> anyhow::Result<()> {
        let path = auth_file()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = json!({
            "access_token": tokens.access_token,
            "refresh_token": tokens.refresh_token,
            "expires_in": tokens.expires_in,
            "saved_at": chrono::Utc::now().to_rfc3339(),
        });
        super::write_private(&path, &body.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    pub fn load_tokens() -> anyhow::Result<Option<TokenSet>> {
        let path = auth_file()?;
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Ok(None);
        };
        let value: Value = serde_json::from_str(&text)?;
        Ok(Some(parse_token_response(&value)?))
    }

    /// Extract `code` and `state` from a callback URL query. A callback
    /// carrying no `state` at all fails validation when one was expected —
    /// "state-validated" must not mean "validated only when present".
    pub fn parse_callback_url(url: &str, expected_state: &str) -> anyhow::Result<String> {
        let query = url
            .split_once('?')
            .map(|(_, query)| query)
            .ok_or_else(|| anyhow!("callback URL has no query"))?;
        match classify_query(query, expected_state) {
            CallbackVerdict::Code(code) => Ok(code),
            CallbackVerdict::Failed(message) => Err(anyhow!(message)),
            CallbackVerdict::Stray => Err(anyhow!(
                "callback state missing or mismatched — possible CSRF; aborting"
            )),
        }
    }

    /// ── Z.ai (GLM Coding Plan) browser sign-in ────────────────────────
    ///
    /// Mirrors ZCode's desktop "Individual Plan" sign-in (community
    /// reverse-engineered, e.g. oh-my-pi #6385): an authorization-code
    /// flow with NO PKCE against chat.z.ai, a non-standard JSON token
    /// exchange on zcode.z.ai, then Z.ai's business API provisions a
    /// DURABLE `id.secret` API key (named in the console) that works
    /// exactly like a console-issued `ZAI_API_KEY` — so the result is
    /// saved to the env file, not a token store.
    pub mod zai {
        use super::*;

        /// ZCode's public OAuth client id.
        pub const CLIENT_ID: &str = "client_P8X5CMWmlaRO9gyO-KSqtg";
        pub const AUTHORIZE_URL: &str = "https://chat.z.ai/api/oauth/authorize";
        pub const TOKEN_URL: &str = "https://zcode.z.ai/api/v1/oauth/token";
        pub const BIZ_BASE: &str = "https://api.z.ai";
        /// Exchanges the short-lived OAuth token for a business token.
        pub const BUSINESS_LOGIN_URL: &str = "https://api.z.ai/api/auth/z/login";
        /// The name of the API key we mint (visible in the Z.ai console).
        pub const KEY_NAME: &str = "modron";

        /// A prepared sign-in: show `url`, then wait for the callback.
        #[derive(Clone, Debug, PartialEq, Eq)]
        pub struct ZaiLoginSession {
            pub url: String,
            pub state: String,
            pub port: u16,
        }

        /// Prepare a sign-in: random state + probed callback port + the
        /// authorize URL (no PKCE, no scopes — verbatim what ZCode sends).
        pub fn prepare_login() -> anyhow::Result<ZaiLoginSession> {
            let state = base64url_nopad(&rand_bytes()?);
            let port = super::probe_callback_port();
            let redirect = format!("http://localhost:{port}/callback");
            let encoded = redirect.replace(':', "%3A").replace('/', "%2F");
            Ok(ZaiLoginSession {
                url: format!(
                    "{AUTHORIZE_URL}?redirect_uri={encoded}&response_type=code&client_id={CLIENT_ID}&state={state}"
                ),
                state,
                port,
            })
        }

        /// Z.ai's `{code, msg, data, success}` envelope: the OAuth token
        /// endpoint signals success with code 0, the business API with
        /// 200/success. Bodies without the wrapper pass through.
        fn unwrap_envelope(body: &Value, op: &str) -> anyhow::Result<Value> {
            let code_ok = match body.get("code") {
                None => true,
                Some(Value::Number(n)) => n.as_i64() == Some(0) || n.as_i64() == Some(200),
                Some(Value::String(s)) => s == "0" || s == "200",
                Some(_) => false,
            };
            let success_ok = body.get("success").and_then(Value::as_bool).unwrap_or(true);
            if !code_ok || !success_ok {
                let msg = body
                    .get("msg")
                    .and_then(Value::as_str)
                    .unwrap_or("(no message)");
                return Err(anyhow!("z.ai {op} failed: {msg}"));
            }
            Ok(body.get("data").cloned().unwrap_or_else(|| body.clone()))
        }

        /// Exchange the callback code (non-standard JSON: no grant_type).
        /// Returns the short-lived OAuth access token (+ account email).
        pub async fn exchange_token_at(
            token_url: &str,
            code: &str,
            state: &str,
            port: u16,
        ) -> anyhow::Result<(String, Option<String>)> {
            let client = oauth_client()?;
            let response = client
                .post(token_url)
                .json(&json!({
                    "provider": "zai",
                    "code": code,
                    "redirect_uri": format!("http://localhost:{port}/callback"),
                    "state": state,
                }))
                .send()
                .await?;
            let body: Value = response.json().await?;
            let data = unwrap_envelope(&body, "token exchange")?;
            let access = data
                .pointer("/zai/access_token")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("token response missing access token"))?
                .to_string();
            let email = data
                .pointer("/user/email")
                .and_then(Value::as_str)
                .map(String::from);
            Ok((access, email))
        }

        /// Exchange the OAuth token for a business token (the biz API
        /// rejects the raw OAuth token).
        pub async fn business_login_at(url: &str, oauth_token: &str) -> anyhow::Result<String> {
            let client = oauth_client()?;
            let response = client
                .post(url)
                .json(&json!({ "token": oauth_token }))
                .send()
                .await?;
            let body: Value = response.json().await?;
            let data = unwrap_envelope(&body, "business login")?;
            let token = ["access_token", "accessToken"]
                .iter()
                .find_map(|k| data.get(*k).and_then(Value::as_str))
                .ok_or_else(|| anyhow!("business login returned no access token"))?;
            Ok(token.to_string())
        }

        /// Coerce an api-keys list response (bare array or wrapper) to a vec.
        fn as_key_array(value: &Value) -> Vec<&Value> {
            if let Some(arr) = value.as_array() {
                return arr.iter().collect();
            }
            for field in ["list", "keys", "apiKeys", "records"] {
                if let Some(arr) = value.get(field).and_then(Value::as_array) {
                    return arr.iter().collect();
                }
            }
            Vec::new()
        }

        /// Provision the durable `id.secret` key: resolve the default
        /// organization/project, reuse (or create) the named key, and
        /// fetch its secret via the copy endpoint (list entries mask it).
        pub async fn mint_key_at(
            biz_base: &str,
            biz_token: &str,
            key_name: &str,
        ) -> anyhow::Result<String> {
            let client = oauth_client()?;
            let auth_header = format!("Bearer {biz_token}");

            let customer = client
                .get(format!("{biz_base}/api/biz/customer/getCustomerInfo"))
                .header("Authorization", &auth_header)
                .send()
                .await?
                .json::<Value>()
                .await?;
            let customer = unwrap_envelope(&customer, "customer lookup")?;
            let orgs = customer
                .get("organizations")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let org = orgs
                .iter()
                .find(|o| o.get("isDefault").and_then(Value::as_bool) == Some(true))
                .or_else(|| orgs.first())
                .ok_or_else(|| anyhow!("no organization on the account"))?;
            let projects = org
                .get("projects")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let project = projects
                .iter()
                .find(|p| p.get("isDefault").and_then(Value::as_bool) == Some(true))
                .or_else(|| projects.first())
                .ok_or_else(|| anyhow!("no project on the account"))?;
            let organization_id = org
                .get("organizationId")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("organization carries no organizationId"))?;
            let project_id = project
                .get("projectId")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("project carries no projectId"))?;

            let keys_url = format!(
                "{biz_base}/api/biz/v1/organization/{organization_id}/projects/{project_id}/api_keys"
            );
            let listed = client
                .get(&keys_url)
                .header("Authorization", &auth_header)
                .send()
                .await?
                .json::<Value>()
                .await?;
            let listed = unwrap_envelope(&listed, "api key list")?;
            let existing = as_key_array(&listed)
                .into_iter()
                .find(|k| k.get("name").and_then(Value::as_str) == Some(key_name));
            let key_record = match existing {
                Some(k) => k.clone(),
                None => {
                    let created = client
                        .post(&keys_url)
                        .header("Authorization", &auth_header)
                        .json(&json!({ "name": key_name }))
                        .send()
                        .await?
                        .json::<Value>()
                        .await?;
                    unwrap_envelope(&created, "api key create")?
                }
            };
            let api_key = key_record
                .get("apiKey")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("key provisioning returned no apiKey"))?;

            // The copy endpoint always returns the FULL secret (list
            // entries mask it; the create response is unreliable).
            let copied = client
                .get(format!("{keys_url}/copy/{}", urlencode(api_key)))
                .header("Authorization", &auth_header)
                .send()
                .await?
                .json::<Value>()
                .await?;
            let copied = unwrap_envelope(&copied, "api key copy")?;
            let secret = copied
                .get("secretKey")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("api key copy returned no secretKey"))?;
            Ok(format!("{api_key}.{secret}"))
        }

        /// Minimal path-segment escaping for the key id in the copy URL.
        fn urlencode(s: &str) -> String {
            s.replace('%', "%25")
                .replace('/', "%2F")
                .replace(':', "%3A")
        }

        /// Complete a sign-in after the browser callback: returns the
        /// durable `id.secret` key (save as `ZAI_API_KEY`) + email.
        pub async fn finish(
            code: &str,
            state: &str,
            port: u16,
        ) -> anyhow::Result<(String, Option<String>)> {
            let (access, email) = exchange_token_at(TOKEN_URL, code, state, port).await?;
            let biz = business_login_at(BUSINESS_LOGIN_URL, &access).await?;
            let key = mint_key_at(BIZ_BASE, &biz, KEY_NAME).await?;
            Ok((key, email))
        }
    }

    /// ── ChatGPT (OpenAI Codex) subscription login ─────────────────────
    ///
    /// OpenAI's device flow (what the Codex CLI itself runs; NOT RFC
    /// 8628): POST a JSON `{client_id}` to the usercode endpoint, the
    /// user enters the returned code at the verification URL, polling the
    /// token endpoint yields an `authorization_code` + `code_verifier`
    /// pair (the server supplies the PKCE verifier), and a final
    /// form-encoded authorization_code exchange returns the tokens.
    /// Storage keeps the chatgpt-account-id the backend requires on
    /// every request.
    pub mod chatgpt {
        use super::*;

        /// The Codex CLI's public OAuth client id (codex-rs
        /// login/src/auth/manager.rs).
        pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
        pub const ISSUER: &str = "https://auth.openai.com";
        /// Step 1: request a user code (JSON body `{client_id}`).
        pub const USERCODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
        /// Step 3: poll for the authorization code (JSON body
        /// `{device_auth_id, user_code}`; 403/404 = not approved yet).
        pub const POLL_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
        /// Step 2: where the user types the code.
        pub const VERIFY_URL: &str = "https://auth.openai.com/codex/device";
        /// Step 4: authorization_code exchange (form-encoded).
        pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
        /// The redirect_uri the device-flow exchange must repeat.
        pub const REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
        /// The Responses-API endpoint ChatGPT-plan traffic uses.
        pub const BACKEND_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
        /// User codes live ~15 minutes (the codex-rs poll cap).
        const LOGIN_TIMEOUT_SECS: u64 = 900;

        /// A started device login: show `user_code` + `verification_uri`
        /// to the user, then call [`finish`].
        #[derive(Clone, Debug, PartialEq, Eq)]
        pub struct DeviceLogin {
            pub device_auth_id: String,
            pub user_code: String,
            pub verification_uri: String,
            /// Poll interval the server asked for (seconds).
            pub interval: u64,
        }

        /// Start the sign-in; returns the code the user enters at
        /// `verification_uri`.
        pub async fn start() -> anyhow::Result<DeviceLogin> {
            begin_at(USERCODE_URL).await
        }

        /// Wait for the browser sign-in; returns the tokens plus the
        /// chatgpt-account-id extracted from the id token.
        pub async fn finish(login: &DeviceLogin) -> anyhow::Result<(TokenSet, String)> {
            finish_at(POLL_URL, TOKEN_URL, login).await
        }

        /// Endpoint-parameterized [`start`] (tests point it at a mock).
        pub async fn begin_at(usercode_url: &str) -> anyhow::Result<DeviceLogin> {
            let client = oauth_client()?;
            let response = client
                .post(usercode_url)
                .json(&json!({ "client_id": CLIENT_ID }))
                .send()
                .await?
                .error_for_status()?;
            let body: Value = response.json().await?;
            let user_code = body
                .get("user_code")
                .or_else(|| body.get("usercode"))
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    anyhow!(
                        "usercode response missing user_code ({})",
                        describe_body(&body)
                    )
                })?;
            Ok(DeviceLogin {
                device_auth_id: body
                    .get("device_auth_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        anyhow!(
                            "usercode response missing device_auth_id ({})",
                            describe_body(&body)
                        )
                    })?
                    .to_string(),
                user_code: user_code.to_string(),
                verification_uri: VERIFY_URL.to_string(),
                // The server sends interval as a string; accept a number.
                interval: body
                    .get("interval")
                    .and_then(|v| {
                        v.as_u64()
                            .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
                    })
                    .unwrap_or(5),
            })
        }

        /// Endpoint-parameterized [`finish`] (tests point it at a mock).
        pub async fn finish_at(
            poll_url: &str,
            token_url: &str,
            login: &DeviceLogin,
        ) -> anyhow::Result<(TokenSet, String)> {
            let client = oauth_client()?;
            let deadline =
                std::time::Instant::now() + std::time::Duration::from_secs(LOGIN_TIMEOUT_SECS);
            loop {
                if std::time::Instant::now() >= deadline {
                    return Err(anyhow!("device login expired (15 min) — run it again"));
                }
                let response = client
                    .post(poll_url)
                    .json(&json!({
                        "device_auth_id": login.device_auth_id,
                        "user_code": login.user_code,
                    }))
                    .send()
                    .await?;
                let status = response.status();
                // 403/404 = the user hasn't approved yet: keep polling
                // (codex-rs device_code_auth.rs semantics).
                if status.as_u16() == 403 || status.as_u16() == 404 {
                    let nap = std::time::Duration::from_secs(login.interval.max(1))
                        .min(deadline.saturating_duration_since(std::time::Instant::now()));
                    tokio::time::sleep(nap).await;
                    continue;
                }
                if !status.is_success() {
                    // The body may carry tokens: report its shape only.
                    let body = response.text().await.unwrap_or_default();
                    return Err(anyhow!(
                        "device auth failed with status {status} ({})",
                        describe_text(&body)
                    ));
                }
                let body: Value = response.json().await?;
                let authorization_code = body
                    .get("authorization_code")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        anyhow!(
                            "poll response missing authorization_code ({})",
                            describe_body(&body)
                        )
                    })?;
                let code_verifier = body
                    .get("code_verifier")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        anyhow!(
                            "poll response missing code_verifier ({})",
                            describe_body(&body)
                        )
                    })?;
                let response = client
                    .post(token_url)
                    .form(&[
                        ("grant_type", "authorization_code"),
                        ("code", authorization_code),
                        ("redirect_uri", REDIRECT_URI),
                        ("client_id", CLIENT_ID),
                        ("code_verifier", code_verifier),
                    ])
                    .send()
                    .await?
                    .error_for_status()?;
                let body: Value = response.json().await?;
                let tokens = parse_token_response(&body)?;
                let id_token = tokens
                    .id_token
                    .as_deref()
                    .ok_or_else(|| anyhow!("token response missing id_token"))?;
                // Account id: nested (or legacy top-level) JWT claim,
                // with the token response body as a last resort.
                let account = account_id(id_token)
                    .or_else(|| {
                        body.get("account_id")
                            .and_then(Value::as_str)
                            .map(String::from)
                    })
                    .ok_or_else(|| anyhow!("id token carries no chatgpt_account_id claim"))?;
                return Ok((tokens, account));
            }
        }

        /// Extract `chatgpt_account_id` from an OIDC id token. Current
        /// OpenAI tokens nest it under the `https://api.openai.com/auth`
        /// claim object; older ones carried it top-level (both accepted).
        /// Signature NOT verified — the token came from the token
        /// endpoint over TLS, and we only read our own account id.
        pub fn account_id(id_token: &str) -> Option<String> {
            let payload_b64 = id_token.split('.').nth(1)?;
            let decoded = base64url_decode(payload_b64)?;
            let value: Value = serde_json::from_slice(&decoded).ok()?;
            if let Some(id) = value.get("chatgpt_account_id").and_then(Value::as_str) {
                return Some(id.to_string());
            }
            value
                .get("https://api.openai.com/auth")
                .and_then(|auth| auth.get("chatgpt_account_id"))
                .and_then(Value::as_str)
                .map(String::from)
        }

        fn base64url_decode(data: &str) -> Option<Vec<u8>> {
            const TABLE: &[u8; 64] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
            let mut vals = Vec::with_capacity(data.len());
            for ch in data.bytes() {
                if ch == b'=' {
                    continue;
                }
                vals.push(TABLE.iter().position(|&t| t == ch)? as u32);
            }
            let mut out = Vec::with_capacity(vals.len() * 3 / 4 + 3);
            for chunk in vals.chunks(4) {
                let b0 = chunk[0];
                let b1 = *chunk.get(1).unwrap_or(&0);
                let b2 = *chunk.get(2).unwrap_or(&0);
                let b3 = *chunk.get(3).unwrap_or(&0);
                let n = (b0 << 18) | (b1 << 12) | (b2 << 6) | b3;
                out.push((n >> 16) as u8);
                if chunk.len() > 2 {
                    out.push((n >> 8) as u8);
                }
                if chunk.len() > 3 {
                    out.push(n as u8);
                }
            }
            Some(out)
        }

        /// Stored ChatGPT credentials: what the Responses provider needs.
        #[derive(Clone, PartialEq, Eq)]
        pub struct ChatGptAuth {
            pub access_token: String,
            pub refresh_token: Option<String>,
            pub account_id: String,
        }

        impl std::fmt::Debug for ChatGptAuth {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct("ChatGptAuth")
                    .field("access_token", &"<redacted>")
                    .field("refresh_token", &redacted_opt(&self.refresh_token))
                    .field("account_id", &self.account_id)
                    .finish()
            }
        }

        pub fn token_file() -> anyhow::Result<PathBuf> {
            let home = std::env::var("HOME").map_err(|_| anyhow!("no home directory"))?;
            Ok(std::path::PathBuf::from(home)
                .join(".modron")
                .join("auth")
                .join("openai.json"))
        }

        pub fn save(auth: &ChatGptAuth) -> anyhow::Result<()> {
            let path = token_file()?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let body = json!({
                "access_token": auth.access_token,
                "refresh_token": auth.refresh_token,
                "account_id": auth.account_id,
                "saved_at": chrono::Utc::now().to_rfc3339(),
            });
            super::super::write_private(&path, &body.to_string())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            }
            Ok(())
        }

        pub fn load() -> anyhow::Result<Option<ChatGptAuth>> {
            let path = token_file()?;
            let Ok(text) = std::fs::read_to_string(&path) else {
                return Ok(None);
            };
            let value: Value = serde_json::from_str(&text)?;
            let access_token = value
                .get("access_token")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("stored openai tokens missing access_token"))?
                .to_string();
            // An empty account id would go out as an empty
            // `chatgpt-account-id` header and fail every request with an
            // opaque 401 — name the fix instead.
            let account_id = value
                .get("account_id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow!("stored openai tokens missing account_id"))?
                .to_string();
            Ok(Some(ChatGptAuth {
                access_token,
                refresh_token: value
                    .get("refresh_token")
                    .and_then(Value::as_str)
                    .map(String::from),
                account_id,
            }))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use serde_json::json;

        #[test]
        fn pkce_challenge_matches_rfc7636_vector() {
            // RFC 7636 appendix B verifier/challenge pair.
            let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
            let (v, challenge) = pkce_pair(Some(verifier)).unwrap();
            assert_eq!(v, verifier);
            assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        }

        #[test]
        fn random_verifier_is_in_bounds_and_challenge_is_urlsafe() {
            let (_, challenge) = pkce_pair(None).unwrap();
            assert!(
                !challenge.contains('+') && !challenge.contains('/') && !challenge.contains('=')
            );
        }

        #[test]
        fn prepared_login_carries_url_and_secret_verifier() {
            let session = prepare_login().unwrap();
            assert!(session.url.starts_with("https://claude.ai/oauth/authorize"));
            assert!(session.url.contains("code_challenge_method=S256"));
            assert!(session.verifier.len() >= 43);
            assert!(!session.url.contains(&session.verifier));
        }

        #[test]
        fn token_response_parses() {
            let tokens = parse_token_response(
                &json!({"access_token": "at", "refresh_token": "rt", "expires_in": 3600}),
            )
            .unwrap();
            assert_eq!(tokens.access_token, "at");
            assert_eq!(tokens.refresh_token.as_deref(), Some("rt"));
            assert_eq!(tokens.expires_in, Some(3600));
            assert!(parse_token_response(&json!({})).is_err());
        }

        #[test]
        fn chatgpt_account_id_extracts_from_jwt() {
            // Legacy top-level claim.
            let payload = base64url_nopad(br#"{"chatgpt_account_id":"acc-123"}"#);
            let jwt = format!("header.{payload}.signature");
            assert_eq!(chatgpt::account_id(&jwt).as_deref(), Some("acc-123"));
            // Current tokens nest it under the auth namespace.
            let nested = base64url_nopad(
                br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acc-456"}}"#,
            );
            let jwt = format!("header.{nested}.signature");
            assert_eq!(chatgpt::account_id(&jwt).as_deref(), Some("acc-456"));
            assert_eq!(chatgpt::account_id("not-a-jwt"), None);
            assert_eq!(chatgpt::account_id("a.b.c"), None); // payload not JSON
        }

        #[tokio::test]
        async fn chatgpt_device_flow_polls_until_tokens() {
            use std::io::{Read, Write};
            use std::net::TcpListener;
            // Scripted server speaking the codex deviceauth protocol: a
            // usercode grant, a 403 "not approved yet" poll, the approval
            // poll (server-provided PKCE pair), the token exchange.
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let usercode_url = format!("http://{addr}/deviceauth/usercode");
            let poll_url = format!("http://{addr}/deviceauth/token");
            let token_url = format!("http://{addr}/oauth/token");
            let jwt = format!(
                "h.{}.s",
                base64url_nopad(br#"{"chatgpt_account_id":"acc-9"}"#)
            );
            let server = std::thread::spawn(move || {
                let respond = |status: &str, body: String| {
                    let (mut sock, _) = listener.accept().unwrap();
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf);
                    let resp = format!(
                        "{status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(resp.as_bytes());
                };
                respond(
                    "HTTP/1.1 200 OK",
                    r#"{"device_auth_id":"da-1","user_code":"ABCD-EFGH","interval":"1"}"#.into(),
                );
                respond("HTTP/1.1 403 Forbidden", String::new());
                respond(
                    "HTTP/1.1 200 OK",
                    r#"{"authorization_code":"ac-1","code_challenge":"cc","code_verifier":"cv-1"}"#
                        .into(),
                );
                respond(
                    "HTTP/1.1 200 OK",
                    format!(
                        r#"{{"access_token":"at-1","refresh_token":"rt-1","id_token":"{jwt}"}}"#
                    ),
                );
            });

            let login = chatgpt::begin_at(&usercode_url).await.unwrap();
            assert_eq!(login.user_code, "ABCD-EFGH");
            assert_eq!(login.device_auth_id, "da-1");
            assert_eq!(login.interval, 1);
            assert_eq!(login.verification_uri, chatgpt::VERIFY_URL);

            let (tokens, account) = chatgpt::finish_at(&poll_url, &token_url, &login)
                .await
                .unwrap();
            assert_eq!(tokens.access_token, "at-1");
            assert_eq!(tokens.refresh_token.as_deref(), Some("rt-1"));
            assert_eq!(account, "acc-9");
            server.join().unwrap();
        }

        #[tokio::test]
        async fn chatgpt_device_flow_fatals_on_hard_errors() {
            use std::io::{Read, Write};
            use std::net::TcpListener;
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let poll_url = format!("http://{addr}/deviceauth/token");
            std::thread::spawn(move || {
                let respond = |status: &str, body: &str| {
                    let (mut sock, _) = listener.accept().unwrap();
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf);
                    let resp = format!(
                        "{status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(resp.as_bytes());
                };
                // A 404 (not approved yet) then a 400 (rejected): only
                // 403/404 keep the poll loop alive.
                respond("HTTP/1.1 404 Not Found", "");
                respond("HTTP/1.1 400 Bad Request", r#"{"error":"access_denied"}"#);
            });
            let login = chatgpt::DeviceLogin {
                device_auth_id: "da".into(),
                user_code: "U".into(),
                verification_uri: chatgpt::VERIFY_URL.into(),
                interval: 1,
            };
            let err = chatgpt::finish_at(&poll_url, "http://127.0.0.1:1/token", &login)
                .await
                .unwrap_err();
            assert!(err.to_string().contains("400"), "{err}");
            // The body is described by field NAME only, never echoed.
            assert!(err.to_string().contains("error"), "{err}");
            assert!(!err.to_string().contains("access_denied"), "{err}");
        }

        #[test]
        fn callback_state_is_validated_and_errors_surface() {
            assert_eq!(
                parse_callback_url("http://localhost:1/callback?code=abc&state=s1", "s1").unwrap(),
                "abc"
            );
            assert!(
                parse_callback_url("http://localhost:1/callback?code=abc&state=evil", "s1")
                    .is_err()
            );
            assert!(parse_callback_url(
                "http://localhost:1/callback?error=access_denied&state=s1",
                "s1"
            )
            .is_err());
        }

        #[tokio::test]
        async fn token_exchange_is_custom_json_with_state_on_the_wire() {
            use std::io::{Read, Write};
            use std::net::TcpListener;
            // Anthropic's token endpoint is CUSTOM: JSON body (not the
            // RFC form encoding) WITH the state field — omitting either
            // gets "Invalid request format" (learned live twice).
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let token_url = format!("http://{addr}/oauth/token");
            let (tx, rx) = std::sync::mpsc::channel::<String>();
            std::thread::spawn(move || {
                let (mut sock, _) = listener.accept().unwrap();
                let mut buf = [0u8; 8192];
                let n = sock.read(&mut buf).unwrap();
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let body = r#"{"access_token":"at-2","refresh_token":"rt-2"}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes());
                let _ = tx.send(request);
            });
            let session = LoginSession {
                url: String::new(),
                state: "st-1".into(),
                verifier: "v".repeat(50),
                port: 54545,
            };
            let tokens = exchange_code_at(&token_url, &session, "code-1")
                .await
                .unwrap();
            assert_eq!(tokens.access_token, "at-2");
            let request = rx.recv().unwrap();
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("content-type: application/json"),
                "json content type: {request}"
            );
            assert!(
                request.contains(r#""grant_type":"authorization_code""#),
                "{request}"
            );
            assert!(request.contains(r#""code":"code-1""#), "{request}");
            assert!(
                request.contains(r#""state":"st-1""#),
                "state is required: {request}"
            );
            assert!(
                request.contains(r#""redirect_uri":"http://localhost:54545/callback""#),
                "{request}"
            );
            assert!(request.contains(r#""code_verifier":"vvvv"#), "{request}");
        }

        #[test]
        fn zai_authorize_url_matches_zcode_shape() {
            let session = zai::prepare_login().unwrap();
            assert!(
                session
                    .url
                    .starts_with("https://chat.z.ai/api/oauth/authorize?"),
                "{}",
                session.url
            );
            assert!(
                session.url.contains("response_type=code"),
                "{}}}",
                session.url
            );
            assert!(
                session
                    .url
                    .contains("client_id=client_P8X5CMWmlaRO9gyO-KSqtg"),
                "{}",
                session.url
            );
            // No PKCE, no scopes — verbatim ZCode shape.
            assert!(!session.url.contains("code_challenge"), "{}}}", session.url);
            assert!(!session.url.contains("scope"), "{}}}", session.url);
            assert!(session.url.contains(&format!("state={}", session.state)));
        }

        #[tokio::test]
        async fn zai_flow_mints_durable_key_on_the_wire() {
            use std::io::{Read, Write};
            use std::net::TcpListener;
            // Scripted sequence: token exchange (envelope code 0), business
            // login (code 200), customer info, empty key list, key create,
            // key copy (secret).
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let token_url = format!("http://{addr}/oauth/token");
            let biz_login = format!("http://{addr}/auth/z/login");
            let biz_base = format!("http://{addr}");
            let (tx, rx) = std::sync::mpsc::channel::<String>();
            std::thread::spawn(move || {
                let respond = |status: &str, body: String| {
                    let (mut sock, _) = listener.accept().unwrap();
                    let mut buf = [0u8; 8192];
                    let n = sock.read(&mut buf).unwrap();
                    let request = String::from_utf8_lossy(&buf[..n]).to_string();
                    let resp = format!(
                        "{status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(resp.as_bytes());
                    request
                };
                let r = respond(
                    "HTTP/1.1 200 OK",
                    r#"{"code":0,"data":{"zai":{"access_token":"oa-1"},"user":{"email":"dev@example.com","id":7}}}"#.into(),
                );
                let _ = tx.send(r);
                let r = respond(
                    "HTTP/1.1 200 OK",
                    r#"{"code":200,"success":true,"data":{"access_token":"biz-1"}}"#.into(),
                );
                let _ = tx.send(r);
                let r = respond(
                    "HTTP/1.1 200 OK",
                    r#"{"code":200,"data":{"organizations":[{"organizationId":"org-9","isDefault":true,"projects":[{"projectId":"proj-1","isDefault":true}]}]}}"#.into(),
                );
                let _ = tx.send(r);
                let r = respond("HTTP/1.1 200 OK", r#"{"code":200,"data":[]}"#.into());
                let _ = tx.send(r);
                let r = respond(
                    "HTTP/1.1 200 OK",
                    r#"{"code":200,"data":{"apiKey":"id123","name":"modron"}}"#.into(),
                );
                let _ = tx.send(r);
                let r = respond(
                    "HTTP/1.1 200 OK",
                    r#"{"code":200,"data":{"secretKey":"sec456"}}"#.into(),
                );
                let _ = tx.send(r);
            });

            let (access, email) = zai::exchange_token_at(&token_url, "c-1", "st", 54545)
                .await
                .unwrap();
            assert_eq!(access, "oa-1");
            assert_eq!(email.as_deref(), Some("dev@example.com"));

            let biz = zai::business_login_at(&biz_login, &access).await.unwrap();
            assert_eq!(biz, "biz-1");

            let key = zai::mint_key_at(&biz_base, &biz, "modron").await.unwrap();
            assert_eq!(key, "id123.sec456", "durable id.secret form");

            // Wire-shape assertions: non-standard exchange body, bearer
            // auth on the business calls, org/project paths, copy URL.
            let mut requests = Vec::new();
            while let Ok(r) = rx.recv_timeout(std::time::Duration::from_millis(300)) {
                requests.push(r);
            }
            let all = requests.join("\n---\n");
            assert!(
                all.contains(r#""provider":"zai""#),
                "non-standard exchange body: {all}"
            );
            assert!(
                all.to_ascii_lowercase()
                    .contains("authorization: bearer biz-1"),
                "{all}"
            );
            assert!(
                all.contains("/api/biz/v1/organization/org-9/projects/proj-1/api_keys"),
                "{all}"
            );
            assert!(
                all.contains("/api/biz/v1/organization/org-9/projects/proj-1/api_keys/copy/id123"),
                "{all}"
            );
            assert!(all.contains("/api/biz/customer/getCustomerInfo"), "{all}");
        }

        #[tokio::test]
        async fn token_refresh_rotates_on_the_wire() {
            use std::io::{Read, Write};
            use std::net::TcpListener;
            // Refresh grant: JSON {grant_type: refresh_token, refresh_token,
            // client_id}; the response carries the ROTATED pair.
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let token_url = format!("http://{addr}/oauth/token");
            let (tx, rx) = std::sync::mpsc::channel::<String>();
            std::thread::spawn(move || {
                let (mut sock, _) = listener.accept().unwrap();
                let mut buf = [0u8; 8192];
                let n = sock.read(&mut buf).unwrap();
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let body =
                    r#"{"access_token":"at-3","refresh_token":"rt-ROTATED","expires_in":28800}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes());
                let _ = tx.send(request);
            });
            let tokens = refresh_tokens_at(&token_url, "rt-old").await.unwrap();
            assert_eq!(tokens.access_token, "at-3");
            assert_eq!(tokens.refresh_token.as_deref(), Some("rt-ROTATED"));
            assert_eq!(tokens.expires_in, Some(28800));
            let request = rx.recv().unwrap();
            assert!(
                request.contains(r#""grant_type":"refresh_token""#),
                "{request}"
            );
            assert!(request.contains(r#""refresh_token":"rt-old""#), "{request}");
            assert!(request.contains(r#""client_id":"9d1c250a"#), "{request}");
        }

        /// Serve one canned HTTP response; returns the base URL.
        fn one_shot_http(response: &'static str) -> String {
            use std::io::{Read, Write};
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            std::thread::spawn(move || {
                if let Ok((mut sock, _)) = listener.accept() {
                    let mut buf = [0u8; 8192];
                    let _ = sock.read(&mut buf);
                    let _ = sock.write_all(response.as_bytes());
                }
            });
            format!("http://{addr}")
        }

        #[test]
        fn secret_fields_never_appear_in_debug_output() {
            let tokens = TokenSet {
                access_token: "SECRET-AT-1".into(),
                refresh_token: Some("SECRET-RT-1".into()),
                expires_in: Some(28800),
                id_token: Some("SECRET-ID-1".into()),
            };
            let dbg = format!("{tokens:?}");
            for secret in ["SECRET-AT-1", "SECRET-RT-1", "SECRET-ID-1"] {
                assert!(!dbg.contains(secret), "{dbg}");
            }
            assert!(dbg.contains("28800"), "{dbg}");

            let login = LoginSession {
                url: "https://claude.ai/oauth/authorize?state=SECRET-STATE-1".into(),
                state: "SECRET-STATE-1".into(),
                verifier: "SECRET-VERIFIER-1".into(),
                port: 54545,
            };
            let dbg = format!("{login:?}");
            assert!(!dbg.contains("SECRET-STATE-1"), "{dbg}");
            assert!(!dbg.contains("SECRET-VERIFIER-1"), "{dbg}");
            assert!(dbg.contains("54545"), "{dbg}");

            let stored = chatgpt::ChatGptAuth {
                access_token: "SECRET-GPT-AT".into(),
                refresh_token: Some("SECRET-GPT-RT".into()),
                account_id: "acct-visible".into(),
            };
            let dbg = format!("{stored:?}");
            assert!(!dbg.contains("SECRET-GPT"), "{dbg}");
            assert!(dbg.contains("acct-visible"), "{dbg}");
        }

        #[test]
        fn token_response_errors_never_echo_the_body() {
            let err = parse_token_response(&json!({
                "refresh_token": "SECRET-RT-ECHO",
                "error": "invalid_grant"
            }))
            .unwrap_err()
            .to_string();
            assert!(!err.contains("SECRET-RT-ECHO"), "{err}");
            assert!(err.contains("access_token"), "{err}");
        }

        #[tokio::test]
        async fn device_poll_errors_never_echo_the_body() {
            let poll = one_shot_http(
                "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: 54\r\nConnection: close\r\n\r\n{\"error\":\"server_error\",\"secret\":\"SECRET-POLL-BODY-1\"}",
            );
            let login = chatgpt::DeviceLogin {
                device_auth_id: "dev-1".into(),
                user_code: "ABCD-1234".into(),
                verification_uri: chatgpt::VERIFY_URL.into(),
                interval: 1,
            };
            let err = chatgpt::finish_at(&format!("{poll}/poll"), "http://127.0.0.1:9/t", &login)
                .await
                .unwrap_err()
                .to_string();
            assert!(!err.contains("SECRET-POLL-BODY-1"), "{err}");
            assert!(err.contains("500"), "{err}");
        }

        /// Blocking raw GET against the callback listener; returns the
        /// whole response (the server closes after answering).
        fn raw_get(addr: std::net::SocketAddr, target: &str) -> String {
            use std::io::{Read, Write};
            let mut sock = std::net::TcpStream::connect(addr).unwrap();
            write!(sock, "GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
            let mut out = String::new();
            let _ = sock.read_to_string(&mut out);
            out
        }

        #[tokio::test]
        async fn callback_stray_requests_do_not_abort_and_codes_are_decoded() {
            let (v4, v6) = bind_callback_listeners(0).await.unwrap();
            let addr = v4.local_addr().unwrap();
            let client = std::thread::spawn(move || {
                let favicon = raw_get(addr, "/favicon.ico");
                assert!(favicon.starts_with("HTTP/1.1 400"), "{favicon}");
                let wrong_state = raw_get(addr, "/callback?code=stolen&state=attacker");
                assert!(wrong_state.starts_with("HTTP/1.1 400"), "{wrong_state}");
                assert!(!wrong_state.contains("Authorized"), "{wrong_state}");
                let real = raw_get(addr, "/callback?code=the%2Bcode%3D&state=S-123");
                assert!(real.starts_with("HTTP/1.1 200"), "{real}");
                assert!(real.contains("Authorized"), "{real}");
            });
            let code = await_callback(v4, v6, "S-123", std::time::Duration::from_secs(10))
                .await
                .unwrap();
            assert_eq!(code, "the+code=");
            client.join().unwrap();
        }

        #[tokio::test]
        async fn callback_wrong_state_never_completes_the_login() {
            let (v4, v6) = bind_callback_listeners(0).await.unwrap();
            let addr = v4.local_addr().unwrap();
            let client = std::thread::spawn(move || {
                raw_get(
                    addr,
                    "/callback?code=stolen&state=attacker&error=access_denied",
                )
            });
            let err = await_callback(v4, v6, "S-123", std::time::Duration::from_millis(400))
                .await
                .unwrap_err();
            assert!(err.to_string().contains("timed out"), "{err}");
            assert!(client.join().unwrap().starts_with("HTTP/1.1 400"));
        }

        #[tokio::test]
        async fn callback_times_out_with_a_clear_error_when_nothing_arrives() {
            let (v4, v6) = bind_callback_listeners(0).await.unwrap();
            let err = await_callback(v4, v6, "S-123", std::time::Duration::from_millis(50))
                .await
                .unwrap_err();
            assert!(err.to_string().contains("timed out"), "{err}");
        }

        #[tokio::test]
        async fn callback_authorization_error_with_valid_state_fails_the_login() {
            let (v4, v6) = bind_callback_listeners(0).await.unwrap();
            let addr = v4.local_addr().unwrap();
            let client = std::thread::spawn(move || {
                raw_get(addr, "/callback?error=access_denied&state=S-123")
            });
            let err = await_callback(v4, v6, "S-123", std::time::Duration::from_secs(10))
                .await
                .unwrap_err();
            assert!(err.to_string().contains("access_denied"), "{err}");
            assert!(client.join().unwrap().starts_with("HTTP/1.1 400"));
        }

        #[test]
        fn callback_query_values_are_percent_decoded() {
            assert_eq!(
                parse_callback_url(
                    "http://localhost:1/callback?code=a%2Fb%3D%2B&state=s1",
                    "s1"
                )
                .unwrap(),
                "a/b=+"
            );
            // A malformed escape is not our callback.
            assert!(
                parse_callback_url("http://localhost:1/callback?code=a%ZZ&state=s1", "s1").is_err()
            );
        }
    }
}
