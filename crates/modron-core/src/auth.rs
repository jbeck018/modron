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
    /// browser with a plain "return to your terminal" page.
    pub async fn wait_for_callback(port: u16, expected_state: &str) -> anyhow::Result<String> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
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

    /// Exchange the callback code for tokens (authorization_code grant).
    pub async fn exchange_code(session: &LoginSession, code: &str) -> anyhow::Result<TokenSet> {
        let client = reqwest::Client::new();
        let response = client
            .post(TOKEN_URL)
            .json(&build_token_exchange(code, &session.verifier, session.port))
            .send()
            .await?;
        let body: Value = response.json().await?;
        parse_token_response(&body)
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
        })
    }

    pub fn build_token_exchange(code: &str, verifier: &str, port: u16) -> Value {
        json!({
            "grant_type": "authorization_code",
            "code": code,
            "redirect_uri": format!("http://localhost:{port}/callback"),
            "client_id": CLIENT_ID,
            "code_verifier": verifier,
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
    }
}
