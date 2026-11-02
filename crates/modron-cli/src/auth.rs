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
    writeln!(
        stdout,
        "   g  ChatGPT (OpenAI)      sign in with your ChatGPT plan"
    )?;
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
    if choice.eq_ignore_ascii_case("g") {
        return chatgpt_login().await;
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
async fn chatgpt_login() -> anyhow::Result<()> {
    use modron_core::auth::oauth::chatgpt;
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
        "tokens stored — use `--provider anthropic` with `[auth.anthropic] use_subscription = true` in config."
    );
    Ok(())
}
