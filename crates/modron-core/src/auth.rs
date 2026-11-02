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
    matches!(provider_id, "anthropic" | "openai-compat")
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
    fn subscription_providers_lead_the_registry() {
        let reg = provider_registry();
        assert_eq!(reg[0].provider_id, "anthropic");
        assert_eq!(reg[1].provider_id, "openai-compat");
        // Only those two offer browser sign-in.
        for info in &reg {
            assert_eq!(
                oauth_capable(info.provider_id),
                matches!(info.provider_id, "anthropic" | "openai-compat"),
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
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct LoginSession {
        pub url: String,
        pub state: String,
        pub verifier: String,
        pub port: u16,
    }

    /// Prepare a login: PKCE pair + state + the authorize URL. Pure — no
    /// sockets, no network; callers show `url` before waiting.
    pub fn prepare_login() -> anyhow::Result<LoginSession> {
        let (verifier, challenge) = pkce_pair(None)?;
        let state = base64url_nopad(&rand_bytes()?);
        let port = CALLBACK_PORT;
        Ok(LoginSession {
            url: authorize_url(&challenge, &state, port),
            state,
            verifier,
            port,
        })
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
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
        if let Some(url) = open_url {
            super::open_browser(url);
        }
        let (mut socket, _) = listener.accept().await?;
        let mut buf = vec![0u8; 16384];
        let n = socket.read(&mut buf).await?;
        let request = String::from_utf8_lossy(&buf[..n]).to_string();
        let request_line = request.lines().next().unwrap_or("");
        // "GET /callback?code=…&state=… HTTP/1.1"
        let target = request_line.split_whitespace().nth(1).unwrap_or("");
        let callback_url = format!("http://localhost:{port}{target}");
        let _ = socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 37\r\nConnection: close\r\n\r\nAuthorized. Return to your terminal.\r\n",
            )
            .await;
        parse_callback_url(&callback_url, expected_state)
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
        let client = reqwest::Client::new();
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
        // JSON body, and the body is the diagnostic worth surfacing.
        let body: Value = response.json().await?;
        parse_token_response(&body)
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
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct TokenSet {
        pub access_token: String,
        pub refresh_token: Option<String>,
        pub expires_in: Option<u64>,
        /// OIDC id token when the flow returns one (OpenAI carries the
        /// chatgpt_account_id claim here).
        pub id_token: Option<String>,
    }

    pub fn parse_token_response(body: &Value) -> anyhow::Result<TokenSet> {
        let access_token = body
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("token response missing access_token: {body}"))?
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
        std::fs::write(&path, body.to_string())?;
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

    /// Extract `code` and `state` from a callback URL query.
    pub fn parse_callback_url(url: &str, expected_state: &str) -> anyhow::Result<String> {
        let query = url
            .split('?')
            .nth(1)
            .ok_or_else(|| anyhow!("callback URL has no query"))?;
        let mut code = None;
        for pair in query.split('&') {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            match k {
                "code" => code = Some(v.to_string()),
                "state" if v != expected_state => {
                    return Err(anyhow!("state mismatch — possible CSRF; aborting"));
                }
                "error" => return Err(anyhow!("authorization failed: {v}")),
                _ => {}
            }
        }
        code.ok_or_else(|| anyhow!("callback URL missing code"))
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
            let client = reqwest::Client::new();
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
                .ok_or_else(|| anyhow!("usercode response missing user_code: {body}"))?;
            Ok(DeviceLogin {
                device_auth_id: body
                    .get("device_auth_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("usercode response missing device_auth_id: {body}"))?
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
            let client = reqwest::Client::new();
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
                    let body = response.text().await.unwrap_or_default();
                    return Err(anyhow!("device auth failed with status {status}: {body}"));
                }
                let body: Value = response.json().await?;
                let authorization_code = body
                    .get("authorization_code")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("poll response missing authorization_code: {body}"))?;
                let code_verifier = body
                    .get("code_verifier")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("poll response missing code_verifier: {body}"))?;
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
        #[derive(Clone, Debug, PartialEq, Eq)]
        pub struct ChatGptAuth {
            pub access_token: String,
            pub refresh_token: Option<String>,
            pub account_id: String,
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
            std::fs::write(&path, body.to_string())?;
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
            let account_id = value
                .get("account_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
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
            assert!(err.to_string().contains("access_denied"), "{err}");
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
    }
}
