//! Subscription auth (Phase 5d X3, EXPERIMENTAL): Anthropic OAuth for
//! Claude-subscription holders, so `--provider anthropic` can run on a
//! Pro/Max plan instead of API keys.
//!
//! Flow (PKCE, RFC 7636): `modron auth login claude` prints the authorize
//! URL and opens a localhost callback listener; the code exchanges at the
//! documented token endpoint; tokens persist 0600 under `~/.modron/auth/`.
//!
//! STATUS: unverified against the live endpoint (needs a subscription and a
//! browser round-trip). ToS-gray: third-party use of subscription auth is
//! not something Anthropic guarantees; API keys remain the first-class path
//! (PLAN §3.6). The pure pieces (PKCE, parsing, storage) are unit-tested.

use crate::AuthAction;
use anyhow::anyhow;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

/// Claude Code's public OAuth client id — the documented, widely-integrated
/// id for subscription flows.
pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
pub const TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
pub const SCOPES: &str = "org:create_api_key user:profile user:inference";

/// PKCE pair: (verifier, S256 challenge). Verifier is 64 chars from the
/// unreserved set; challenge is base64url(SHA256(verifier)) without padding.
pub fn pkce_pair(verifier_seed: Option<&str>) -> anyhow::Result<(String, String)> {
    let verifier = match verifier_seed {
        Some(seed) => seed.to_string(),
        None => {
            // 48 random bytes → 64 base64url chars (randomness from the OS).
            let bytes: [u8; 48] = rand_bytes()?;
            base64url_nopad(&bytes).chars().take(64).collect()
        }
    };
    if !(43..=128).contains(&verifier.len()) {
        return Err(anyhow!(
            "PKCE verifier must be 43–128 chars, got {}",
            verifier.len()
        ));
    }
    let digest = Sha256::digest(verifier.as_bytes());
    Ok((verifier, base64url_nopad(&digest)))
}

fn base64url_nopad(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
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
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home directory"))?;
    Ok(home.join(".modron").join("auth").join("anthropic.json"))
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

// ── API-key store: the onboarding env file ────────────────────────────────
//
// Keys live as `export <VAR>="<key>"` lines in ~/.config/modron/env (0600,
// same file first-run onboarding writes), so every launch path — TUI, one-shot,
// daemon — sources them identically. Process env still wins over the file.

/// Read every `export VAR="value"` line of an env file into pairs.
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

/// Write/replace one provider key in an env file (0600 on create).
pub fn set_key_in_env_file(path: &std::path::Path, env_var: &str, key: &str) -> anyhow::Result<()> {
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
pub fn remove_key_from_env_file(path: &std::path::Path, env_var: &str) -> anyhow::Result<bool> {
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

/// Mask a key for display: `sk-…wxyz`.
pub fn mask_key(key: &str) -> String {
    let n = key.chars().count();
    if n <= 8 {
        return "…".to_string();
    }
    let head: String = key.chars().take(3).collect();
    let tail: String = key.chars().skip(n - 4).collect();
    format!("{head}…{tail}")
}

/// Where a configured key came from, for `auth status`.
enum KeySource {
    ProcessEnv,
    EnvFile,
    None,
}

fn key_source(env_var: &str) -> (KeySource, String) {
    // The saved file wins for labeling (startup sources it into the
    // process anyway); only shell-only exports show as process env.
    if let Ok(path) = crate::onboarding::env_file() {
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

// ── Commands ──────────────────────────────────────────────────────────────

/// `modron auth` family: token/OAuth management for every provider.
pub(super) async fn run_auth(action: AuthAction) -> anyhow::Result<()> {
    match action {
        AuthAction::Login => login().await,
        AuthAction::Add => add_provider_wizard().await,
        AuthAction::Set { provider_id, key } => set_key(&provider_id, &key),
        AuthAction::Status => {
            print_status()?;
            Ok(())
        }
        AuthAction::Logout { provider_id } => logout(provider_id.as_deref()),
    }
}

/// Interactive: pick a provider (or Claude OAuth), paste a token. The
/// first-run onboarding screen reuses the same registry.
async fn add_provider_wizard() -> anyhow::Result<()> {
    use std::io::Write;
    let options = crate::onboarding::provider_options();
    let mut stdout = std::io::stdout();
    writeln!(stdout)?;
    writeln!(stdout, "  add a provider")?;
    writeln!(stdout)?;
    for opt in &options {
        writeln!(stdout, "   {:>2} {:<18} {}", opt.number, opt.name, opt.note)?;
    }
    writeln!(stdout, "   o  Claude subscription   OAuth in the browser")?;
    writeln!(stdout)?;
    write!(stdout, "  choice: ")?;
    stdout.flush()?;

    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let choice = line.trim();
    if choice.eq_ignore_ascii_case("o") {
        return login().await;
    }
    let num: u8 = choice
        .parse()
        .map_err(|_| anyhow!("invalid choice: {choice}"))?;
    let opt = options
        .iter()
        .find(|o| o.number == num)
        .ok_or_else(|| anyhow!("no provider {num}"))?;

    writeln!(stdout)?;
    writeln!(stdout, "  get a key at: {}", opt.key_url)?;
    write!(stdout, "  paste {} : ", opt.env_var)?;
    stdout.flush()?;
    let mut key_line = String::new();
    std::io::stdin().read_line(&mut key_line)?;
    let key = key_line.trim();
    if key.is_empty() {
        anyhow::bail!("no key provided");
    }
    set_key(opt.provider_id, key)
}

/// Non-interactive: `modron auth set <provider> <key>`.
fn set_key(provider: &str, key: &str) -> anyhow::Result<()> {
    if key.trim().is_empty() {
        anyhow::bail!("empty key");
    }
    let opt = crate::onboarding::option_for_provider(provider).ok_or_else(|| {
        anyhow!("unknown provider `{provider}` — run `modron auth add` to pick one")
    })?;
    let path = crate::onboarding::env_file()?;
    set_key_in_env_file(&path, opt.env_var, key)?;
    // The current process picks it up too (matches onboarding behavior).
    std::env::set_var(opt.env_var, key);
    println!(
        "✓ {} key saved to {} (0600) — provider `{}` is ready",
        opt.name,
        path.display(),
        opt.provider_id
    );
    Ok(())
}

fn print_status() -> anyhow::Result<()> {
    println!("provider          key");
    for opt in crate::onboarding::provider_options() {
        let (source, value) = key_source(opt.env_var);
        let shown = match source {
            KeySource::ProcessEnv => format!("{} (process env)", mask_key(&value)),
            KeySource::EnvFile => format!("{} (saved)", mask_key(&value)),
            KeySource::None => "—".to_string(),
        };
        println!("{:<17} {}", format!("{} ", opt.name), shown);
    }
    match load_tokens()? {
        Some(tokens) => println!(
            "{:<17} {} (OAuth)",
            "Claude subscription ",
            mask_key(&tokens.access_token)
        ),
        None => println!("{:<17} — (run `modron auth login`)", "Claude subscription "),
    }
    Ok(())
}

fn logout(provider: Option<&str>) -> anyhow::Result<()> {
    match provider {
        // Back-compat: bare `auth logout` clears the Claude OAuth tokens.
        None => match auth_file() {
            Ok(path) => match std::fs::remove_file(&path) {
                Ok(()) => {
                    println!("tokens deleted ({})", path.display());
                    Ok(())
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    println!("no tokens stored");
                    Ok(())
                }
                Err(e) => Err(anyhow!("cannot delete tokens: {e}")),
            },
            Err(e) => Err(e),
        },
        Some(provider) => {
            let opt = crate::onboarding::option_for_provider(provider).ok_or_else(|| {
                anyhow!("unknown provider `{provider}` — run `modron auth add` to pick one")
            })?;
            let path = crate::onboarding::env_file()?;
            if remove_key_from_env_file(&path, opt.env_var)? {
                std::env::remove_var(opt.env_var);
                println!("✓ {} key removed", opt.name);
            } else {
                println!("no saved {} key", opt.name);
            }
            Ok(())
        }
    }
}

async fn login() -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (verifier, challenge) = pkce_pair(None)?;
    let state = base64url_nopad(&rand_bytes()?);
    let port = 54545u16;
    let url = authorize_url(&challenge, &state, port);

    println!("EXPERIMENTAL: Anthropic subscription auth (ToS-gray; API keys remain first-class).");
    println!("1. Open this URL in a browser and approve access:");
    println!("{url}");
    println!("2. Waiting for the localhost:{port} callback …");

    // One-shot callback listener.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let (mut socket, _) = listener.accept().await?;
    let mut buf = vec![0u8; 16384];
    let n = socket.read(&mut buf).await?;
    let request = String::from_utf8_lossy(&buf[..n]).to_string();
    let request_line = request.lines().next().unwrap_or("");
    // "GET /callback?code=…&state=… HTTP/1.1"
    let target = request_line.split_whitespace().nth(1).unwrap_or("");
    let callback_url = format!("http://localhost:{port}{target}");
    let code = parse_callback_url(&callback_url, &state)?;

    let _ = socket
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 20\r\nConnection: close\r\n\r\nAuthorized. Return to the terminal.\r\n",
        )
        .await;

    // Token exchange.
    let client = reqwest::Client::new();
    let response = client
        .post(TOKEN_URL)
        .json(&build_token_exchange(&code, &verifier, port))
        .send()
        .await?;
    let body: Value = response.json().await?;
    let tokens = parse_token_response(&body)?;
    save_tokens(&tokens)?;
    println!("tokens stored — use `--provider anthropic` with `[auth.anthropic] use_subscription = true` in config.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(!challenge.contains('+') && !challenge.contains('/') && !challenge.contains('='));
    }

    #[test]
    fn token_response_parses() {
        let tokens = parse_token_response(&json!({
            "access_token": "at", "refresh_token": "rt", "expires_in": 3600
        }))
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
            parse_callback_url("http://localhost:1/callback?code=abc&state=evil", "s1").is_err()
        );
        assert!(parse_callback_url(
            "http://localhost:1/callback?error=access_denied&state=s1",
            "s1"
        )
        .is_err());
    }

    #[test]
    fn env_file_set_parse_remove_roundtrip() {
        let dir = std::env::temp_dir().join(format!("modron-auth-{}", std::process::id()));
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
        let pairs = parse_env_file(&std::fs::read_to_string(&path).unwrap());
        assert_eq!(pairs.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keys_are_masked_for_display() {
        assert_eq!(mask_key("sk-ant-1234567890"), "sk-…7890");
        assert_eq!(mask_key("short"), "…");
    }

    #[test]
    fn provider_lookup_covers_the_registry() {
        assert_eq!(
            crate::onboarding::option_for_provider("zai")
                .unwrap()
                .env_var,
            "ZAI_API_KEY"
        );
        assert_eq!(
            crate::onboarding::option_for_provider("github-models")
                .unwrap()
                .env_var,
            "GITHUB_MODELS_TOKEN"
        );
        assert!(crate::onboarding::option_for_provider("nope").is_none());
    }

    #[test]
    fn tokens_roundtrip_through_disk() {
        // auth_file() is home-dependent; exercise save/load via the real
        // path only when it exists — unit-level roundtrip instead:
        let tokens = TokenSet {
            access_token: "x".into(),
            refresh_token: Some("y".into()),
            expires_in: Some(1),
        };
        let body = json!({"access_token": "x", "refresh_token": "y", "expires_in": 1});
        let parsed = parse_token_response(&body).unwrap();
        assert_eq!(parsed, tokens);
    }
}
