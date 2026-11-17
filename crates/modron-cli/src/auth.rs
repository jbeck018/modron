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

// OAuth (PKCE, localhost callback, token storage) lives in
// `modron_core::auth::oauth` — shared with the TUI's interactive flow.

// ── Commands ──────────────────────────────────────────────────────────────

/// `modron auth` family: token/OAuth management for every provider.
pub(super) async fn run_auth(action: AuthAction, provider: Option<&str>) -> anyhow::Result<()> {
    match action {
        AuthAction::Login => match login_route(provider)? {
            LoginRoute::Claude => login().await,
            LoginRoute::ChatGpt => chatgpt_login().await,
        },
        AuthAction::Add => add_provider_wizard().await,
        AuthAction::Set { provider_id, key } => {
            let key = read_key_arg(key)?;
            set_key(&provider_id, &key)
        }
        AuthAction::Status => {
            print_status()?;
            Ok(())
        }
        AuthAction::Logout { provider_id } => logout(provider_id.as_deref()),
    }
}

/// The sign-in `modron auth login` runs. The global `--provider` flag picks
/// it; omitted keeps the Claude subscription login it has always run.
#[derive(Debug, PartialEq, Eq)]
enum LoginRoute {
    Claude,
    ChatGpt,
}

fn login_route(provider: Option<&str>) -> anyhow::Result<LoginRoute> {
    match provider {
        None | Some("anthropic") => Ok(LoginRoute::Claude),
        Some("chatgpt") => Ok(LoginRoute::ChatGpt),
        Some(other) => Err(anyhow!(
            "`auth login` signs in to anthropic or chatgpt, not `{other}` \
             (API keys: `modron auth set {other}`)"
        )),
    }
}

/// The `auth status` cell for the ChatGPT sign-in: masked token only.
fn chatgpt_status(
    stored: anyhow::Result<Option<modron_core::auth::oauth::chatgpt::ChatGptAuth>>,
) -> String {
    match stored {
        Ok(Some(auth)) => format!(
            "{} (OAuth)",
            modron_core::auth::mask_key(&auth.access_token)
        ),
        Ok(None) => "— (run `modron auth login --provider chatgpt`)".to_string(),
        Err(e) => format!("unreadable ({e}) — run `modron auth login --provider chatgpt`"),
    }
}

/// Resolve the `auth set` key argument: an explicit value wins; omitted or
/// `-` reads one line from stdin — piped secrets never touch shell history
/// or `ps` output.
fn read_key_arg(key: Option<String>) -> anyhow::Result<String> {
    match key {
        Some(k) if k != "-" => Ok(k),
        _ => {
            use std::io::BufRead as _;
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line)?;
            let key = line.trim().to_string();
            if key.is_empty() {
                anyhow::bail!("no key provided — pass it on stdin or as the argument");
            }
            Ok(key)
        }
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
    writeln!(
        stdout,
        "   g  ChatGPT (OpenAI)      sign in with your ChatGPT plan"
    )?;
    writeln!(stdout, "   o  Claude subscription   OAuth in the browser")?;
    writeln!(
        stdout,
        "   z  Z.ai (GLM Plan)      sign in — mints an API key"
    )?;
    writeln!(stdout)?;
    write!(stdout, "  choice: ")?;
    stdout.flush()?;

    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let choice = line.trim();
    if choice.eq_ignore_ascii_case("o") {
        return login().await;
    }
    if choice.eq_ignore_ascii_case("g") {
        return chatgpt_login().await;
    }
    if choice.eq_ignore_ascii_case("z") {
        return zai_login().await;
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

/// Web search keys for the `web_search` tool, accepted by `auth set` next to
/// the providers. They are not providers: no chat model, no wizard entry.
/// Returns (display name, env var) for a search id.
fn search_key_env(id: &str) -> Option<(&'static str, &'static str)> {
    match id {
        "brave" => Some(("Brave Search", "BRAVE_API_KEY")),
        "tavily" => Some(("Tavily", "TAVILY_API_KEY")),
        "exa" => Some(("Exa", "EXA_API_KEY")),
        _ => None,
    }
}

/// Non-interactive: `modron auth set <provider> <key>`.
fn set_key(provider: &str, key: &str) -> anyhow::Result<()> {
    if key.trim().is_empty() {
        anyhow::bail!("empty key");
    }
    if let Some((name, env_var)) = search_key_env(provider) {
        let path = crate::onboarding::env_file()?;
        modron_core::auth::set_key_in_env_file(&path, env_var, key)?;
        std::env::set_var(env_var, key);
        println!(
            "✓ {name} key saved to {} (0600); web_search uses it when no earlier key is set",
            path.display()
        );
        return Ok(());
    }
    let opt = crate::onboarding::option_for_provider(provider).ok_or_else(|| {
        anyhow!("unknown provider `{provider}` — run `modron auth add` to pick one")
    })?;
    let path = crate::onboarding::env_file()?;
    modron_core::auth::set_key_in_env_file(&path, opt.env_var, key)?;
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
        let (source, value) = modron_core::auth::key_source(opt.env_var);
        let shown = match source {
            modron_core::auth::KeySource::ProcessEnv => {
                format!("{} (process env)", modron_core::auth::mask_key(&value))
            }
            modron_core::auth::KeySource::EnvFile => {
                format!("{} (saved)", modron_core::auth::mask_key(&value))
            }
            modron_core::auth::KeySource::None => "—".to_string(),
        };
        println!("{:<17} {}", format!("{} ", opt.name), shown);
    }
    match modron_core::auth::oauth::load_tokens()? {
        Some(tokens) => println!(
            "{:<17} {} (OAuth)",
            "Claude subscription ",
            modron_core::auth::mask_key(&tokens.access_token)
        ),
        None => println!("{:<17} — (run `modron auth login`)", "Claude subscription "),
    }
    println!(
        "{:<17} {}",
        "ChatGPT sign-in ",
        chatgpt_status(modron_core::auth::oauth::chatgpt::load())
    );
    Ok(())
}

fn logout(provider: Option<&str>) -> anyhow::Result<()> {
    match provider {
        // Back-compat: bare `auth logout` clears the Claude OAuth tokens.
        None => match modron_core::auth::oauth::auth_file() {
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
        // The ChatGPT sign-in is OAuth tokens, not a saved key: clear the
        // token file and leave OPENAI_API_KEY alone.
        Some("chatgpt") => {
            use modron_core::auth::oauth::chatgpt;
            if chatgpt::clear()? {
                println!("✓ ChatGPT sign-in removed");
            } else {
                println!("no ChatGPT sign-in stored");
            }
            Ok(())
        }
        Some(provider) => {
            let opt = crate::onboarding::option_for_provider(provider).ok_or_else(|| {
                anyhow!("unknown provider `{provider}` — run `modron auth add` to pick one")
            })?;
            let path = crate::onboarding::env_file()?;
            if modron_core::auth::remove_key_from_env_file(&path, opt.env_var)? {
                std::env::remove_var(opt.env_var);
                println!("✓ {} key removed", opt.name);
            } else {
                println!("no saved {} key", opt.name);
            }
            Ok(())
        }
    }
}

/// Terminal ChatGPT device flow: prints the code + URL, polls until the
/// browser sign-in completes, saves tokens.
/// Z.ai browser sign-in: mints a durable API key saved as ZAI_API_KEY.
async fn zai_login() -> anyhow::Result<()> {
    use modron_core::auth::oauth::zai;
    let session = zai::prepare_login()?;
    println!("1. Opening the Z.ai sign-in page in your browser (or paste manually):");
    println!("{}", session.url);
    println!("2. Waiting for the localhost:{} callback …", session.port);

    let code = modron_core::auth::oauth::wait_for_callback(
        session.port,
        &session.state,
        Some(&session.url),
    )
    .await?;
    let (key, email) = zai::finish(&code, &session.state, session.port).await?;
    let path =
        modron_core::auth::env_file().map_err(|e| anyhow!("cannot resolve env file: {e}"))?;
    modron_core::auth::set_key_in_env_file(&path, "ZAI_API_KEY", &key)?;
    std::env::set_var("ZAI_API_KEY", &key);
    println!(
        "✓ z.ai signed in{} — GLM Coding Plan key saved ({}) — use `--provider zai`",
        email.map(|e| format!(" ({e})")).unwrap_or_default(),
        modron_core::auth::mask_key(&key)
    );
    Ok(())
}

/// ChatGPT subscription sign-in (Codex device flow). Used by
/// `auth login --provider chatgpt`, the `auth add` wizard and onboarding.
/// Tokens land in `~/.modron/auth/openai.json`, the same store the Codex
/// import writes and the provider refreshes.
pub(crate) async fn chatgpt_login() -> anyhow::Result<()> {
    use modron_core::auth::oauth::chatgpt;
    println!(
        "EXPERIMENTAL: ChatGPT subscription sign-in (ToS-gray; OpenAI API keys remain first-class)."
    );
    let device = chatgpt::start().await?;
    modron_core::auth::open_browser(&device.verification_uri);
    println!(
        "1. Opening {} in your browser — enter this code there:",
        device.verification_uri
    );
    println!("   code: {}", device.user_code);
    println!("   (if nothing opened: {})", device.verification_uri);
    println!("2. Waiting for the sign-in…");
    let (tokens, account) = chatgpt::finish(&device).await?;
    chatgpt::save(&chatgpt::ChatGptAuth {
        access_token: tokens.access_token.clone(),
        refresh_token: tokens.refresh_token.clone(),
        account_id: account,
    })?;
    println!(
        "✓ chatgpt signed in ({}) — use `--provider chatgpt` (models gpt-6-astra, gpt-5.6-sol)",
        modron_core::auth::mask_key(&tokens.access_token)
    );
    Ok(())
}

async fn login() -> anyhow::Result<()> {
    let session = modron_core::auth::oauth::prepare_login()?;

    println!("EXPERIMENTAL: Anthropic subscription auth (ToS-gray; API keys remain first-class).");
    println!("1. Opening the approval page in your browser (or paste manually):");
    println!("{}", session.url);
    println!("2. Waiting for the localhost:{} callback …", session.port);

    let code = modron_core::auth::oauth::wait_for_callback(
        session.port,
        &session.state,
        Some(&session.url),
    )
    .await?;
    let tokens = modron_core::auth::oauth::exchange_code(&session, &code).await?;
    modron_core::auth::oauth::save_tokens(&tokens)?;
    println!(
        "tokens stored — `--provider anthropic` now rides the subscription automatically \
         (a saved API key stays the fallback; /auth → Anthropic → remove to go back)"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{chatgpt_status, login_route, search_key_env, LoginRoute};
    use modron_core::auth::oauth::chatgpt::ChatGptAuth;

    #[test]
    fn login_routes_by_provider_and_defaults_to_claude() {
        assert_eq!(login_route(None).unwrap(), LoginRoute::Claude);
        assert_eq!(login_route(Some("anthropic")).unwrap(), LoginRoute::Claude);
        assert_eq!(login_route(Some("chatgpt")).unwrap(), LoginRoute::ChatGpt);
        let err = login_route(Some("zai")).unwrap_err().to_string();
        assert!(err.contains("not `zai`"), "{err}");
        assert!(err.contains("modron auth set zai"), "{err}");
    }

    #[test]
    fn chatgpt_status_shows_a_masked_token_and_never_the_raw_one() {
        let token = "eyJhbGciOiJSUzI1NiJ9.payload-secret-value.sig";
        let signed_in = chatgpt_status(Ok(Some(ChatGptAuth {
            access_token: token.into(),
            refresh_token: Some("rt-secret".into()),
            account_id: "acc-1".into(),
        })));
        assert!(signed_in.ends_with("(OAuth)"), "{signed_in}");
        assert!(!signed_in.contains(token), "{signed_in}");
        assert!(!signed_in.contains("rt-secret"), "{signed_in}");

        let absent = chatgpt_status(Ok(None));
        assert!(
            absent.contains("modron auth login --provider chatgpt"),
            "{absent}"
        );

        let broken = chatgpt_status(Err(anyhow::anyhow!(
            "stored openai tokens missing account_id"
        )));
        assert!(broken.contains("unreadable"), "{broken}");
    }

    #[test]
    fn chatgpt_is_not_a_key_provider_id() {
        // `logout chatgpt` must not fall through to the key path, which
        // rejects unknown ids (the ChatGPT registry id is openai-compat).
        assert!(crate::onboarding::option_for_provider("chatgpt").is_none());
    }

    #[test]
    fn search_ids_map_to_the_web_search_env_vars() {
        assert_eq!(
            search_key_env("brave").map(|(_, v)| v),
            Some("BRAVE_API_KEY")
        );
        assert_eq!(
            search_key_env("tavily").map(|(_, v)| v),
            Some("TAVILY_API_KEY")
        );
        assert_eq!(search_key_env("exa").map(|(_, v)| v), Some("EXA_API_KEY"));
        assert!(
            search_key_env("zai").is_none(),
            "providers keep their own path"
        );
    }
}
