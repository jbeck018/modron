//! modron-cli — one-shot terminal front end for the Modron agent.
//!
//! LEDGER items: L1 (clap CLI wiring — this file, owned by the lead
//! architect), L2 (config — `config.rs`, wired in here), L3 (root README),
//! X3 (compaction flags + progress line), K3 (cache-hit column), M4
//! (`modron memory` subcommand — Phase 1a).
//!
//! Flow: load config → pick provider/model (flag > config > engine default)
//! → build provider (mock = scripted demo, live providers wrapped in retry)
//! → run the engine loop → print the final answer and an estimated-cost
//! usage table with the provider-reported cache-hit rate. Progress (turn
//! starts, tool calls, compactions) goes to stderr via `tracing`/direct
//! writes; the answer and usage table go to stdout.

pub mod acp;
pub mod daemon;
pub mod onboarding;
use daemon as modron_daemon;
pub mod auth;
pub mod config;
pub mod evals;
pub mod upgrade;

use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use modron_core::memory::MemoryStore;
use modron_core::{
    estimate_tokens, CompactionPolicy, Engine, EngineConfig, ModelProvider, SessionEvent,
    SessionId, SessionStore, StreamDelta, ToolCall, Usage,
};
use modron_providers::{provider_from_env, RetryingProvider};
use modron_store::JsonlSessionStore;
use modron_tools::{default_registry_with_memory, VerifyTool};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// `--provider` choices. Ids match the provider factory and engine registry
/// keys ("anthropic", "openai-compat", "mock").
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum ProviderChoice {
    Anthropic,
    /// Any OpenAI-compatible endpoint (OpenAI, OpenRouter, DeepSeek, Ollama…).
    OpenaiCompat,
    /// ChatGPT subscription sign-in (Responses API on the ChatGPT
    /// backend; tokens from `/auth` → OpenAI → sign in with browser).
    Chatgpt,
    /// Keyless scripted demo provider (no network, runs the built-in script).
    Mock,
    /// Z.ai (Zhipu) GLM models — OpenAI-compatible, key via `ZAI_API_KEY`.
    Zai,
}

impl ProviderChoice {
    fn id(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenaiCompat => "openai-compat",
            Self::Chatgpt => "chatgpt",
            Self::Mock => "mock",
            Self::Zai => "zai",
        }
    }
}

/// `--sandbox` choices (A2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum SandboxChoice {
    /// No OS sandbox (default) — the approval gate is the only control.
    Off,
    /// macOS Seatbelt: network denied, writes fenced to workspace + temp.
    Seatbelt,
    /// Linux bubblewrap: network denied, workspace read-write, rest read-only.
    Bubblewrap,
}

impl SandboxChoice {
    fn policy(self) -> modron_core::SandboxPolicy {
        match self {
            Self::Off => modron_core::SandboxPolicy::Off,
            Self::Seatbelt => modron_core::SandboxPolicy::Seatbelt,
            Self::Bubblewrap => modron_core::SandboxPolicy::Bubblewrap,
        }
    }
}

#[derive(Subcommand, Clone)]
enum Command {
    /// Interactive multi-turn session: type tasks, stream answers, one
    /// shared session (`/exit` to quit, Ctrl-D also works).
    Chat,
    /// Full terminal UI: inline viewport, live tool lines, esc-to-interrupt,
    /// inline approval prompts (y/n/a).
    Tui,
    /// Router tooling (offline): score the selector against the held-out
    /// eval set and report accuracy + estimated cost delta vs. always-strong.
    Router {
        #[command(subcommand)]
        action: RouterAction,
    },
    /// Agent Client Protocol server for editors (Zed first): JSON-RPC over
    /// stdio. Editors launch this binary themselves.
    Acp,
    /// Mini-SWE evals: run the committed task suite through the real engine
    /// in isolated workspaces and score with objective verifiers.
    Eval {
        /// Comma-separated model list (default: one run on the configured model).
        #[arg(long)]
        models: Option<String>,
        /// Only run eval tasks whose id contains this substring.
        #[arg(long)]
        filter: Option<String>,
    },
    /// Upgrade the binary from the latest GitHub release.
    Upgrade,
    /// Diagnose your setup: keys, config, git, PDF support, terminal.
    Doctor,
    /// Manage the background engine daemon (sessions survive terminal close).
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Run the remote-control session server: WebSocket + local Unix socket
    /// (Phase 2). Sessions live here; clients attach from anywhere.
    Serve {
        /// WebSocket listen address, or "disabled" for Unix-socket only.
        /// Loopback binds trust local clients; anything else requires a
        /// paired device token (`modron pair`).
        #[arg(long, default_value = "127.0.0.1:7699")]
        listen: String,
        /// Remote-control bridge base (wss://host) used by /publish.
        #[arg(long)]
        bridge: Option<String>,
    },
    /// Manage paired remote devices (Phase 3): mint, list, revoke the
    /// bearer tokens remote clients send in their hello frame.
    Pair {
        /// Device name for `modron pair <name>` (mint + print a token).
        /// Omit when using a subcommand.
        name: Option<String>,
        #[command(subcommand)]
        action: Option<PairAction>,
    },
    /// Manage provider credentials: API keys for any provider, OAuth for
    /// Claude subscriptions.
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
    /// Inspect configured MCP servers (tools with schema costs, resources,
    /// prompts) without running the agent.
    Mcp {
        #[command(subcommand)]
        action: McpAction,
    },
    /// Run the fixed benchmark suite across a curated list of models and
    /// report tokens/turns/duration/cost per model (the OpenCode-style
    /// comparison; the default list is Z.ai's free tier).
    Bench {
        /// Comma-separated model list (default: the curated free tier).
        #[arg(long)]
        models: Option<String>,
        /// Skip the suite and just print the curated free-model list.
        #[arg(long)]
        list: bool,
        /// Milliseconds to sleep between tasks (free tiers rate-limit; 3000
        /// is a good default for back-to-back free runs).
        #[arg(long)]
        delay_ms: Option<u64>,
        /// Write the per-model comparison rows (completed fraction, tokens
        /// per task, ms per task, cache %) as a JSON array to this path, in
        /// addition to the printed table.
        #[arg(long, value_name = "PATH")]
        json: Option<PathBuf>,
    },
    /// Inspect and forget persistent workspace memory (no silent state).
    Memory {
        #[command(subcommand)]
        action: MemoryAction,
    },
    /// Undo the last file mutation checkpoint in a session: restores the
    /// file (or deletes it if the agent created it) and marks the transcript
    /// for resume-from-truncation. Bash mutations are NOT checkpointed (v1).
    Rewind {
        /// Session to rewind (default: the most recently updated one).
        #[arg(long)]
        session: Option<SessionId>,
    },
    /// Session management: list (default), vacuum old sessions, restart.
    Sessions {
        #[command(subcommand)]
        action: Option<SessionsAction>,
    },
    /// Per-turn harness trace rows for one session (model, latency, cost,
    /// prompt hash, verifier verdict, stop label) - the TurnTrace reader
    /// (the events were persisted since the AHE pass; nothing read them).
    Traces {
        /// The session id (`modron sessions list` prints them).
        session: String,
    },
    /// Aggregate token/cost dashboard across sessions (provider-reported
    /// usage, catalog-priced costs, cache-hit rates).
    Usage {
        /// Show only the N most recent sessions.
        #[arg(long)]
        last: Option<usize>,
    },
}

#[derive(Subcommand, Clone)]
enum DaemonAction {
    /// Start the daemon in the background.
    Start,
    /// Check if the daemon is running.
    Status,
    /// Stop the daemon.
    Stop,
}

#[derive(Subcommand, Clone)]
enum PairAction {
    /// List paired devices (name + when the token was minted).
    List,
    /// Revoke a device's token (its hello frames stop authenticating).
    Revoke { name: String },
}

#[derive(Subcommand, Clone)]
enum SessionsAction {
    /// List stored sessions (newest first) with tokens + est. cost.
    List,
    /// Delete sessions not updated within the given number of days.
    Vacuum {
        #[arg(long)]
        older_than_days: u64,
        #[arg(long)]
        dry_run: bool,
    },
    /// Restart a session: same id, fresh context (history stays on disk but
    /// never re-enters context; the next --resume starts clean).
    Restart { id: SessionId },
}

#[derive(Subcommand, Clone)]
enum McpAction {
    /// Inspect one configured server by name.
    Info { name: String },
    /// Read one resource from a configured server (the `read_resource`
    /// capability was unreachable before the 2026-11-12 audit).
    Read {
        /// The configured server name (`modron mcp info <name>` lists URIs).
        name: String,
        /// The resource URI to read.
        uri: String,
    },
}

#[derive(Subcommand, Clone)]
enum AuthAction {
    /// Add a provider interactively: pick one, paste a token (or run the
    /// Claude OAuth flow in the browser).
    Add,
    /// Authorize a Claude subscription via browser (PKCE + localhost
    /// callback) and store tokens.
    Login,
    /// Store an API key without the wizard: `auth set <provider> [key]`.
    /// Omit the key (or pass `-`) to read it from stdin — piped secrets
    /// never touch shell history or `ps` output.
    /// (The field is `provider_id`, not `provider` — the root `--provider`
    /// flag is a global arg and would collide on the arg id.)
    Set {
        provider_id: String,
        key: Option<String>,
    },
    /// Show every provider's configured key (masked) and OAuth state.
    Status,
    /// Delete stored credentials. Bare `logout` clears Claude OAuth
    /// tokens; `logout <provider>` removes that provider's saved key.
    Logout {
        /// Provider id (zai, anthropic, groq, …); omit for Claude OAuth.
        provider_id: Option<String>,
    },
}

#[derive(Subcommand, Clone)]
enum RouterAction {
    /// Score the active selector against the bundled held-out eval set.
    Eval,
}

#[derive(Subcommand, Clone)]
enum MemoryAction {
    /// List the workspace's memory index (newest first).
    List,
    /// Print the body of the newest memory entry titled <title>.
    Show { title: String },
    /// Delete every memory entry titled <title>.
    Forget { title: String },
    /// Show the usefulness ledger (uses/successes/failures/relegated).
    Stats,
    /// Run the curator pass: decay + relegate useless facts, prune stale
    /// breadcrumbs (Phase 13 self-improving memory).
    Curate,
}

#[derive(Parser, Clone)]
#[command(
    name = "modron",
    version,
    about = "Modron — a token-efficient Rust AI coding agent (approvals, sandbox, memory, compaction, TUI)",
    after_help = "Config file: ~/.config/modron/config.toml — Sessions: ~/.modron/sessions/ — Memory: ~/.modron/memory/"
)]
struct Cli {
    /// `modron --upgrade` / `--update` — alias for the `upgrade` subcommand.
    #[arg(long, alias = "update", hide = true)]
    upgrade: bool,
    /// Task text; the agent runs it to completion and prints the answer.
    /// Omit when using a subcommand. If the task itself starts with a
    /// subcommand name, pass `--` first: `modron -- memory leaks in parser`.
    task: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
    /// Provider to use (default: config `default_provider`, else "anthropic").
    #[arg(long, value_enum, global = true)]
    provider: Option<ProviderChoice>,
    /// Model name (default: config `default_model`, else the engine default).
    #[arg(long, global = true)]
    model: Option<String>,
    /// Per-turn assembled-context cap in tokens (pre-flight estimate).
    #[arg(long, global = true)]
    max_input_tokens: Option<u64>,
    /// Cumulative token cap for the whole run (provider-reported usage).
    #[arg(long, global = true)]
    max_total_tokens: Option<u64>,
    /// Maximum agent turns (model calls) for this run.
    #[arg(long, global = true)]
    max_turns: Option<u32>,
    /// Working directory the tools run in.
    #[arg(long, default_value = ".", global = true)]
    working_dir: PathBuf,
    /// Session store directory (default: ~/.modron/sessions). The workspace
    /// memory root is derived as <this dir's parent>/memory.
    #[arg(long, global = true)]
    sessions_dir: Option<PathBuf>,
    /// Natural-language stop condition: keep the run going (same session,
    /// iterative submissions) until the model reports the goal met or a
    /// cap trips. Pairs with a task argument.
    #[arg(long, global = true)]
    goal: Option<String>,

    /// Continue a session: bare --resume picks the most recent; use
    /// --resume=<id> for a specific one. (Claude Code / Codex convention.)
    #[arg(long, global = true, num_args = 0..=1, require_equals = true)]
    resume: Option<Option<SessionId>>,
    /// Drive a running `modron serve` instead of an in-process engine:
    /// "unix" (the default socket), "unix:<path>", or "ws://host:port".
    /// Sessions live on the server and survive this TUI closing; combine
    /// with --resume to reattach.
    #[arg(long, global = true)]
    connect: Option<String>,
    /// Autocompaction trigger: compact when the estimated context exceeds
    /// this fraction of --max-input-tokens (0 < t < 1; default 0.8).
    #[arg(long, global = true)]
    compact_threshold: Option<f64>,
    /// Disable autocompaction for this run.
    #[arg(long, global = true)]
    no_compact: bool,
    /// Approve every mutating tool (bash/write/edit) for this run. Without
    /// it, Ask-mode prompts on the terminal when one is attached and blocks
    /// otherwise (with an explanation on stderr).
    #[arg(long, global = true)]
    approve_all: bool,
    /// Alias for --approve-all (the Claude Code / Codex convention).
    #[arg(long, global = true)]
    yolo: bool,
    /// Run as a named persona from [agents] in config (system prompt, forced
    /// model, tool lists).
    #[arg(long, global = true)]
    agent: Option<String>,
    /// Allow a tool without prompting (repeatable, e.g. --allow bash).
    /// Deny wins over allow.
    #[arg(long, global = true)]
    allow: Vec<String>,
    /// Always block a tool (repeatable, any tool class).
    #[arg(long, global = true)]
    deny: Vec<String>,
    /// USD spend cap for this run, enforced per turn from provider-reported
    /// usage priced by the model catalog. Models missing from the catalog
    /// count as $0 (warned once).
    #[arg(long, global = true)]
    max_cost: Option<f64>,
    /// OS sandbox for mutating tool subprocesses. `seatbelt` (macOS) denies
    /// network access and fences file writes to the workspace + temp dirs —
    /// defense in depth on top of the approval gate. Default: off.
    #[arg(long, value_enum, global = true)]
    sandbox: Option<SandboxChoice>,
    /// Verbose tracing (debug) on stderr; default logs info-level progress.
    #[arg(long, global = true)]
    verbose: bool,
    /// Attach a file to the task (repeatable): images (png/jpg/gif/webp)
    /// are sent as image content; text and PDF files are inlined as
    /// documents. PDF extraction uses pdftotext when installed.
    #[arg(long, global = true)]
    attach: Vec<PathBuf>,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    init_tracing(cli.verbose, tui_session(&cli));
    if let Err(err) = run(cli).await {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

/// Will this invocation run the interactive TUI (bare `modron`, `modron
/// tui`, or post-onboarding on a TTY)? Decided before tracing initializes
/// so logs can be routed away from stderr before the viewport takes over.
fn tui_session(cli: &Cli) -> bool {
    use std::io::IsTerminal as _;
    cli.task.is_none()
        && matches!(cli.command, None | Some(Command::Tui))
        && std::io::stdout().is_terminal()
}

/// Tracing sink: stderr for one-shot/subcommand runs; a log FILE for the
/// TUI. Writing to stderr while the inline viewport owns the terminal
/// corrupts the frame — raw mode means LF without CR and no wrap control,
/// so interleaved "engine turn start" lines staircase over the UI and
/// desync ratatui's scroll tracking (the ghost-box/duplicated-lines bug).
/// TUI logs land in ~/.modron/logs/modron.log.
fn init_tracing(verbose: bool, tui: bool) {
    use std::io::IsTerminal as _;
    let default_filter = if verbose { "debug" } else { "info" };
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| default_filter.to_string());
    if tui {
        let dir = default_sessions_dir()
            .parent()
            .map(|root| root.join("logs"))
            .unwrap_or_else(std::env::temp_dir);
        let file = std::fs::create_dir_all(&dir).ok().and_then(|()| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("modron.log"))
                .ok()
        });
        if let Some(file) = file {
            tracing_subscriber::fmt()
                .with_writer(std::sync::Mutex::new(file))
                .with_ansi(false)
                .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
                .init();
            tracing::info!(
                "modron TUI session — logs routed to file (stderr corrupts the viewport)"
            );
            return;
        }
        // Log file unopenable: protect the TUI — cap stderr at warn.
        let fallback = std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".to_string());
        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .with_env_filter(tracing_subscriber::EnvFilter::new(fallback))
            .init();
        return;
    }
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .init();
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    // `modron --upgrade` — flag alias for the upgrade subcommand.
    if cli.upgrade {
        return crate::upgrade::run_upgrade().await;
    }
    // Source saved API keys from onboarding (Phase 12).
    crate::onboarding::source_env_file();
    let cfg = config::load();

    let sessions_dir = cli
        .sessions_dir
        .clone()
        .unwrap_or_else(default_sessions_dir);
    let working_dir = cli.working_dir.clone();

    // `modron memory …` — inspect/forget user memory; no provider, no run.
    if let Some(Command::Memory { action }) = cli.command {
        return run_memory(action, &sessions_dir, &working_dir);
    }
    // `modron pair …` — device tokens for the remote-control server; no
    // provider, no run.
    if let Some(Command::Pair { name, action }) = &cli.command {
        return run_pair(name.clone(), action.clone());
    }
    // `modron rewind` — restore the last checkpointed file + set the
    // truncation point the next --resume of that session will honor.
    if let Some(Command::Rewind { session }) = cli.command {
        return run_rewind(session, &sessions_dir, &working_dir).await;
    }
    // `modron sessions` — the store is inspectable (no silent state).
    if let Some(Command::Sessions { action }) = &cli.command {
        match action {
            Some(SessionsAction::List) | None => {
                return run_sessions_list(&sessions_dir).await;
            }
            Some(SessionsAction::Vacuum {
                older_than_days,
                dry_run,
            }) => {
                return run_sessions_vacuum(&sessions_dir, *older_than_days, *dry_run).await;
            }
            Some(SessionsAction::Restart { id }) => {
                return run_sessions_restart(&sessions_dir, *id).await;
            }
        }
    }
    if let Some(Command::Traces { session }) = &cli.command {
        return run_traces(&sessions_dir, session).await;
    }
    // `modron usage` — the aggregate dashboard (Phase-2a D1).
    if let Some(Command::Usage { last }) = cli.command {
        return run_usage(&sessions_dir, last).await;
    }

    // `modron router eval` — offline routing report (Phase-3a R4).
    if let Some(Command::Router {
        action: RouterAction::Eval,
    }) = cli.command
    {
        return run_router_eval(&cfg);
    }

    // `modron acp` — editor integration (Phase 5d X1).
    if let Some(Command::Acp) = cli.command {
        return crate::acp::run_acp(&cli, &cfg).await;
    }

    // `modron eval` — mini-SWE scoring (Phase 6c).
    if let Some(Command::Eval { models, filter }) = cli.command.clone() {
        return crate::evals::run_evals(&cli, &cfg, models.as_deref(), filter.as_deref()).await;
    }

    // `modron upgrade` — self-update from the latest release (Phase 6e).
    if let Some(Command::Upgrade) = cli.command {
        return crate::upgrade::run_upgrade().await;
    }

    // `modron doctor` — setup diagnostics (Phase 8).
    if let Some(Command::Doctor) = cli.command {
        return run_doctor(&cfg);
    }

    // `modron daemon …` — background engine management (Phase 9).
    if let Some(Command::Daemon { action }) = &cli.command {
        return run_daemon(action.clone(), &cli, &cfg).await;
    }

    // `modron serve` — the remote-control session server (Phase 2). Built
    // exactly like `daemon start`: same agent path, long-lived process.
    if let Some(Command::Serve { listen, bridge }) = &cli.command {
        let mcp = spawn_mcp_servers(&cfg).await?;
        let agent = build_agent_with(
            &cli,
            &cfg,
            false,
            AgentSinks::for_mode(RunMode::HEADLESS),
            &mcp,
        )?;
        return modron_server::run_serve(
            agent.engine,
            &modron_server::ServeOptions {
                listen: Some(listen.clone()),
                bridge_base: bridge.clone(),
                ..modron_server::ServeOptions::default()
            },
        )
        .await;
    }

    // `modron auth …` — subscription token management (Phase 5d X3).
    if let Some(Command::Auth { action }) = cli.command {
        return crate::auth::run_auth(action).await;
    }

    // `modron mcp info …` — server inspection (Phase 5e).
    if let Some(Command::Mcp {
        action: McpAction::Read { name, uri },
    }) = &cli.command
    {
        return run_mcp_read(&cfg, name.clone(), uri.clone()).await;
    }
    if let Some(Command::Mcp {
        action: McpAction::Info { name },
    }) = cli.command
    {
        return run_mcp_info(&cfg, name).await;
    }

    // `modron bench` — multi-model measurement (Phase 5f).
    if let Some(Command::Bench {
        models,
        list,
        delay_ms,
        json,
    }) = cli.command.clone()
    {
        return run_bench(
            &cli,
            &cfg,
            models.as_deref(),
            list,
            delay_ms,
            json.as_deref(),
        )
        .await;
    }

    // Session TTL auto-vacuum (Phase 6a): runs before anything else when
    // [sessions].ttl_days is configured. Unset = never delete automatically.
    if let Some(sessions_cfg) = &cfg.sessions {
        if let Some(days) = sessions_cfg.ttl_days {
            if let Err(e) = run_sessions_vacuum(&sessions_dir, days, false).await {
                tracing::warn!(error = %e, "session auto-vacuum failed (non-fatal)");
            }
        }
    }

    // `modron chat` — interactive multi-turn REPL (Phase-1c I3).
    if let Some(Command::Chat) = cli.command {
        return run_chat(&cli, &cfg).await;
    }
    // `modron tui` — inline-viewport UI (Phase-1d). The sinks are created
    // inside the TUI, so the agent is built through the closure.
    if let Some(Command::Tui) = cli.command {
        let interactive = std::io::IsTerminal::is_terminal(&std::io::stdin());
        let mcp = spawn_mcp_servers(&cfg).await?;
        let notice = modron_upgrade_notice();
        return modron_tui::run_with(
            |sinks| {
                let build = |cli: &Cli| {
                    build_agent_with(
                        cli,
                        &cfg,
                        interactive,
                        AgentSinks {
                            approver: Some(sinks.approver.clone()),
                            on_delta: Some(sinks.on_delta.clone()),
                            on_tool_event: Some(sinks.on_tool_event.clone()),
                        },
                        &mcp,
                    )
                };
                let agent = match build(&cli) {
                    Ok(agent) => agent,
                    Err(err) => {
                        // No-key startup guarantee: the TUI always opens.
                        eprintln!(
                            "warning: {err:#}\n  opening in mock (demo) mode — \
                             add a key via onboarding or `modron doctor`"
                        );
                        let mut demo = cli.clone();
                        demo.provider = Some(ProviderChoice::Mock);
                        demo.model = None;
                        // The retry can still fail for non-provider reasons
                        // (unknown --agent, bad [router] mode) — surface the
                        // error instead of panicking mid-startup.
                        build(&demo)?
                    }
                };
                Ok((agent.engine, agent.resume))
            },
            String::new(),
            notice,
            cli.connect.as_ref().map(|target| modron_tui::RemoteSpec {
                target: target.clone(),
                resume: cli.resume,
            }),
        )
        .await
        .map_err(|err| anyhow::anyhow!("{err:#}"));
    }

    let Some(task) = cli.task.clone() else {
        // Claude Code / Codex convention: bare `modron` opens the TUI when
        // a terminal is attached; piped invocations get help.
        use std::io::IsTerminal as _;
        if std::io::stdout().is_terminal() {
            let interactive = std::io::stdin().is_terminal();
            // First-run onboarding (Phase 12): if no provider key exists,
            // show the friendly setup screen instead of erroring later.
            // An explicit --provider choice (especially mock) IS a
            // configuration — never intercept it with onboarding.
            if cli.provider.is_none() && cli.connect.is_none() && !onboarding::has_any_key() {
                let provider = onboarding::run_onboarding()?;
                let mut onboarded = cli.clone();
                onboarded.provider = None; // let config drive it
                if provider == "mock" {
                    onboarded.provider = Some(ProviderChoice::Mock);
                }
                let mcp = spawn_mcp_servers(&cfg).await?;
                let notice = modron_upgrade_notice();
                return modron_tui::run_with(
                    |sinks| {
                        let build = |cli: &Cli| {
                            build_agent_with(
                                cli,
                                &cfg,
                                true,
                                AgentSinks {
                                    approver: Some(sinks.approver.clone()),
                                    on_delta: Some(sinks.on_delta.clone()),
                                    on_tool_event: Some(sinks.on_tool_event.clone()),
                                },
                                &mcp,
                            )
                        };
                        let agent = match build(&onboarded) {
                            Ok(agent) => agent,
                            Err(err) => {
                                // No-key startup guarantee: the TUI always opens.
                                eprintln!(
                                    "warning: {err:#}\n  opening in mock (demo) mode — \
                                     add a key via onboarding or `modron doctor`"
                                );
                                let mut demo = onboarded.clone();
                                demo.provider = Some(ProviderChoice::Mock);
                                demo.model = None;
                                build(&demo)?
                            }
                        };
                        Ok((agent.engine, agent.resume))
                    },
                    tui_model_label(&onboarded, &cfg),
                    notice,
                    None,
                )
                .await
                .map_err(|err| anyhow::anyhow!("{err:#}"));
            }
            let mcp = spawn_mcp_servers(&cfg).await?;
            let notice = modron_upgrade_notice();
            let effective_cli = if cli.yolo {
                let mut c = cli.clone();
                c.approve_all = true;
                c
            } else {
                cli.clone()
            };
            return modron_tui::run_with(
                |sinks| {
                    let build = |cli: &Cli| {
                        build_agent_with(
                            cli,
                            &cfg,
                            interactive,
                            AgentSinks {
                                approver: Some(sinks.approver.clone()),
                                on_delta: Some(sinks.on_delta.clone()),
                                on_tool_event: Some(sinks.on_tool_event.clone()),
                            },
                            &mcp,
                        )
                    };
                    let agent = match build(&effective_cli) {
                        Ok(agent) => agent,
                        Err(err) => {
                            // No-key startup guarantee: the TUI always opens.
                            eprintln!(
                                "warning: {err:#}\n  opening in mock (demo) mode — \
                                 add a key via onboarding or `modron doctor`"
                            );
                            let mut demo = effective_cli.clone();
                            demo.provider = Some(ProviderChoice::Mock);
                            demo.model = None;
                            build(&demo)?
                        }
                    };
                    Ok((agent.engine, agent.resume))
                },
                tui_model_label(&cli, &cfg),
                notice,
                cli.connect.as_ref().map(|target| modron_tui::RemoteSpec {
                    target: target.clone(),
                    resume: cli.resume,
                }),
            )
            .await
            .map_err(|err| anyhow::anyhow!("{err:#}"));
        }
        Cli::command().print_help()?;
        return Ok(());
    };

    let interactive = std::io::IsTerminal::is_terminal(&std::io::stdin());
    let mcp = spawn_mcp_servers(&cfg).await?;
    let agent = build_agent_with(
        &cli,
        &cfg,
        interactive,
        AgentSinks::for_mode(if interactive {
            RunMode::INTERACTIVE
        } else {
            RunMode::HEADLESS
        }),
        &mcp,
    )?;
    let compaction_tail_messages = agent.engine.config.compaction.tail_messages;

    // Events already in the log before this invocation — used to scope the
    // usage table and compaction lines to THIS run (a resumed session already
    // carries past usage/compactions). Doubles as an early, actionable check
    // that a --resume id exists.
    // Resolve --resume: explicit id wins; bare --resume picks the latest.
    let resume_session_id: Option<SessionId> = match &cli.resume {
        Some(Some(id)) => Some(*id),
        Some(None) => {
            let sessions = agent
                .store
                .list()
                .await
                .map_err(|e| anyhow::anyhow!("cannot list sessions: {e}"))?;
            match sessions.first() {
                Some(meta) => {
                    eprintln!("resuming: {} ({})", meta.id, meta.title);
                    Some(meta.id)
                }
                None => anyhow::bail!("no sessions found — run a task first, then --resume"),
            }
        }
        None => None,
    };

    let usage_baseline = match resume_session_id {
        Some(session_id) => agent.store.event_count(session_id).await? as usize,
        None => 0,
    };

    let attachments = cli
        .attach
        .iter()
        .map(|p| load_attachment(p))
        .collect::<anyhow::Result<Vec<_>>>()?;
    if !attachments.is_empty() {
        eprintln!(
            "attached: {}",
            attachments
                .iter()
                .map(|a| match a {
                    modron_core::Attachment::Image { .. } => "image",
                    modron_core::Attachment::Document { name, .. } => name,
                })
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let summary = if let Some(goal) = cli.goal.clone() {
        // Long-horizon run (Phase 14): iterate in one session until the
        // natural-language stop condition is reported or a cap trips.
        // Attachments are NOT plumbed through the goal loop yet — fail
        // loudly instead of silently dropping them.
        if !attachments.is_empty() {
            anyhow::bail!("--attach is not supported with --goal yet — run the attachment task without --goal");
        }
        let max_iters = std::env::var("MODRON_GOAL_MAX_ITERATIONS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(modron_core::goal::DEFAULT_MAX_ITERATIONS);
        let outcome = modron_core::goal::run_goal_loop(
            &agent.engine,
            resume_session_id,
            &task,
            &goal,
            max_iters,
        )
        .await?;
        eprintln!(
            "goal run: {} iteration(s) — {}",
            outcome.iterations,
            if outcome.goal_met {
                "goal reported complete"
            } else {
                "stopped before the goal was reported (cap or budget)"
            }
        );
        outcome.summary
    } else {
        match resume_session_id {
            Some(session_id) => agent.engine.run_task_resuming(session_id, &task).await?,
            None => {
                agent
                    .engine
                    .run_task_with_attachments(&task, attachments)
                    .await?
            }
        }
    };

    // One load serves both stderr compaction progress and the usage table.
    let events = agent.store.load(summary.session_id).await?;
    print_compaction_lines(&events, usage_baseline, compaction_tail_messages);

    if summary.answer.is_empty() {
        eprintln!(
            "note: no final answer — the run stopped at a turn/budget limit \
             ({} turns, {} tool calls); see the session transcript",
            summary.turns, summary.tool_calls
        );
    } else {
        println!("{}", summary.answer);
    }

    print_usage_table(&events[usage_baseline..])?;
    println!();
    println!(
        "session: {}  (turns: {}, tool calls: {})",
        summary.session_id, summary.turns, summary.tool_calls
    );
    Ok(())
}

/// `~/.modron/sessions`, with an honest in-cwd fallback when the platform
/// has no home directory.
/// Assembled agent pieces shared by the one-shot run, chat, and the TUI.
struct Agent {
    engine: Engine,
    store: Arc<JsonlSessionStore>,
    resume: Option<SessionId>,
}

/// Injected sinks (Phase-1d): the one-shot run passes all-None (answer at
/// end), chat passes the stdout sink + tty approver, the TUI passes its own
/// channel-backed sinks.
struct AgentSinks {
    approver: Option<Arc<modron_core::Approver>>,
    on_delta: Option<Arc<modron_core::DeltaSink>>,
    on_tool_event: Option<Arc<modron_core::ToolActivitySink>>,
}

impl AgentSinks {
    fn for_mode(mode: RunMode) -> Self {
        Self {
            approver: if mode.interactive {
                tty_approver()
            } else {
                None
            },
            on_delta: if mode.stream {
                Some(stdout_delta_sink())
            } else {
                None
            },
            on_tool_event: None,
        }
    }
}

/// First provider (besides `unavailable`) whose key is set.
///
/// The no-key startup fallback scans common providers in rough preference
/// order.
pub(crate) fn detect_available_provider(unavailable: &str) -> Option<String> {
    let candidates = [
        ("zai", "ZAI_API_KEY"),
        ("anthropic", "ANTHROPIC_API_KEY"),
        ("openai-compat", "OPENAI_API_KEY"),
        ("google", "GEMINI_API_KEY"),
        ("groq", "GROQ_API_KEY"),
        ("openrouter", "OPENROUTER_API_KEY"),
        ("mistral", "MISTRAL_API_KEY"),
        ("cerebras", "CEREBRAS_API_KEY"),
        ("nvidia", "NVIDIA_API_KEY"),
    ];
    candidates
        .iter()
        .find(|(pid, env)| {
            *pid != unavailable && std::env::var(env).map(|v| !v.is_empty()).unwrap_or(false)
        })
        .map(|(pid, _)| pid.to_string())
}

/// The catalog's default model for a provider (first strong-tier entry,
/// else the first entry) — used when a provider switch invalidates the
/// default model.
fn default_model_for_provider(pid: &str) -> Option<String> {
    let entries = modron_router::catalog();
    entries
        .iter()
        .filter(|m| m.provider == pid)
        .find(|m| m.tier == modron_router::Tier::Strong)
        .or_else(|| entries.iter().find(|m| m.provider == pid))
        .map(|m| m.name.clone())
}

/// The TUI status-line model label: explicit flag > config > "mock" for
/// demo runs, else empty (the engine's internal default).
fn tui_model_label(cli: &Cli, cfg: &config::Config) -> String {
    cli.model
        .clone()
        .or_else(|| cfg.default_model.clone())
        .unwrap_or_else(|| {
            if matches!(cli.provider, Some(ProviderChoice::Mock)) {
                "mock".to_string()
            } else {
                String::new()
            }
        })
}

/// Assemble provider + tools + store + config from flags/config (Phase-1c
/// I3 refactor): one construction path for the one-shot run and chat mode.
/// - `interactive`: attach the /dev/tty approver when a terminal is present,
///   so Ask mode prompts instead of blocking.
/// - `stream`: attach the stdout delta sink (chat mode streams answers live).
fn build_agent_with(
    cli: &Cli,
    cfg: &config::Config,
    interactive: bool,
    sinks: AgentSinks,
    mcp: &[McpRegistration],
) -> anyhow::Result<Agent> {
    let engine_defaults = EngineConfig::default();
    let sessions_dir = cli
        .sessions_dir
        .clone()
        .unwrap_or_else(default_sessions_dir);
    let working_dir = cli.working_dir.clone();

    // Flag > config file/env > engine default, per flag.
    let provider_id = cli
        .provider
        .map(|p| p.id().to_string())
        .or_else(|| cfg.default_provider.clone())
        .unwrap_or_else(|| engine_defaults.default_provider.clone());
    let model = cli
        .model
        .clone()
        .or_else(|| cfg.default_model.clone())
        .unwrap_or_else(|| engine_defaults.default_model.clone());

    if let Some(threshold) = cli.compact_threshold {
        if !(threshold > 0.0 && threshold < 1.0) {
            anyhow::bail!("--compact-threshold must be strictly between 0 and 1, got {threshold}");
        }
    }
    // Precedence: engine default < [compaction] in config.toml < CLI flags.
    let mut compaction = CompactionPolicy::default();
    if let Some(section) = &cfg.compaction {
        if let Some(v) = section.enabled {
            compaction.enabled = v;
        }
        if let Some(v) = section.threshold_frac {
            compaction.threshold_frac = v;
        }
        if let Some(v) = section.min_interval_turns {
            compaction.min_interval_turns = v;
        }
        if let Some(v) = section.tail_messages {
            compaction.tail_messages = v;
        }
        if let Some(v) = section.summary_max_bytes {
            compaction.summary_max_bytes = v;
        }
        if let Some(v) = section.bitmap_compaction {
            compaction.bitmap_compaction = v;
        }
    }
    if cli.no_compact {
        compaction.enabled = false;
    }
    if let Some(t) = cli.compact_threshold {
        compaction.threshold_frac = t;
    }

    // Workspace user memory (Phase 1a §4.9): <sessions-parent>/memory, i.e.
    // ~/.modron/memory by default. Same root feeds the `remember` tool and
    // the engine's system-prompt section.
    let memory_root = memory_root_for(&sessions_dir);

    // Same construction as bench's build_provider_shim - one body, not two.
    let build_provider =
        |pid: &str| -> anyhow::Result<Arc<dyn ModelProvider>> { build_provider_shim(pid) };

    // Async decision model (Phase-3b M3): [judge] or [kev] — [judge] wins
    // with a warning if both are set.
    let async_router: Option<(Arc<dyn modron_core::AsyncRouter>, modron_core::JudgeMode)> =
        if let Some(judge) = &cfg.judge {
            let judge_provider_id = judge
                .provider
                .clone()
                .unwrap_or_else(|| provider_id.clone());
            let provider = build_provider(&judge_provider_id)
                .map_err(|err| err.context("cannot start the [judge] provider"))?;
            let cheap_default = cfg
                .router
                .as_ref()
                .and_then(|r| r.cheap_model.clone())
                .or_else(|| default_model_for_provider(&judge_provider_id))
                .unwrap_or_else(|| "claude-haiku-5-5".to_string());
            let model = judge.model.clone().unwrap_or(cheap_default);
            Some((
                Arc::new(modron_providers::LlmJudgeRouter::new(provider, model)),
                parse_judge_mode(judge.mode.as_deref())?,
            ))
        } else if let Some(kev) = &cfg.kev {
            Some((
                Arc::new(modron_providers::KevRouter::new(
                    kev.endpoint.clone(),
                    kev.api_key.clone(),
                    kev.model
                        .clone()
                        .unwrap_or_else(|| "kev-latest".to_string()),
                )),
                parse_judge_mode(kev.mode.as_deref())?,
            ))
        } else {
            None
        };

    let persona = cli
        .agent
        .as_ref()
        .map(|name| {
            let agents = cfg.agents.clone().unwrap_or_default();
            agents.get(name).cloned().ok_or_else(|| {
                let available: Vec<&str> = agents.keys().map(|s| s.as_str()).collect();
                anyhow::anyhow!(
                    "unknown agent `{name}` — configured agents: {}",
                    if available.is_empty() {
                        "(none)".to_string()
                    } else {
                        available.join(", ")
                    }
                )
            })
        })
        .transpose()?;

    let mut engine_config = EngineConfig {
        working_dir: working_dir.clone(),
        system: persona
            .as_ref()
            .and_then(|p| p.system.clone())
            .unwrap_or_default(),
        default_provider: provider_id.clone(),
        default_model: model.clone(),
        tier_fallbacks: None,
        max_turns: cli.max_turns.unwrap_or(engine_defaults.max_turns),
        max_input_tokens: cli
            .max_input_tokens
            .or(cfg.max_input_tokens)
            .unwrap_or(engine_defaults.max_input_tokens),
        max_total_tokens: cli
            .max_total_tokens
            .or(cfg.max_total_tokens)
            .unwrap_or(engine_defaults.max_total_tokens),
        compaction,
        memory_root: Some(memory_root.clone()),
        approvals: {
            let mut approvals = modron_core::ApprovalPolicy {
                mode: if cli.approve_all || cli.yolo {
                    modron_core::ApprovalMode::ApproveAll
                } else {
                    modron_core::ApprovalMode::Ask
                },
                non_interactive: !interactive,
                allow: cli.allow.clone(),
                deny: cli.deny.clone(),
            };
            if let Some(p) = persona.as_ref() {
                approvals.allow.extend(p.allow.clone().unwrap_or_default());
                approvals.deny.extend(p.deny.clone().unwrap_or_default());
            }
            approvals
        },
        max_cost_usd: cli.max_cost,
        price_lookup: Some(Arc::new(|model: &str| {
            modron_router::lookup(model).map(|info| (info.input_per_m, info.output_per_m))
        })),
        // Honors a prior `modron rewind` of this session (C2): abandoned
        // events stay in the log but never re-enter context.
        resume_from_event: cli.resume.flatten().and_then(|id| {
            let marker = sessions_dir.join(format!("{id}.rewind"));
            std::fs::read_to_string(&marker)
                .ok()
                .and_then(|text| text.trim().parse::<u64>().ok())
        }),
        hooks: cfg
            .hooks
            .clone()
            .map(|h| modron_core::hooks::HookConfig {
                pre_tool: h.pre_tool,
                post_tool: h.post_tool,
            })
            .unwrap_or_default(),
        sandbox: {
            let policy = cli
                .sandbox
                .map(|choice| choice.policy())
                .unwrap_or_default();
            if !modron_core::sandbox::is_supported(policy) {
                anyhow::bail!(
                    "sandbox policy {policy:?} is not supported on this platform; \
                     run with --sandbox off (or omit the flag)"
                );
            }
            policy
        },
        approver: if cli.approve_all || cli.yolo {
            None
        } else {
            sinks.approver
        },
        async_router,
        self_check_on_finish: true,
        // config.toml top-level `escalate_on_check_failure = "<model id>"`:
        // one verify-failure escalation turn on that model (None = off).
        escalate_on_check_failure: cfg.escalate_on_check_failure.clone(),
        on_delta: sinks.on_delta,
        on_tool_event: sinks.on_tool_event,
        personas: Default::default(),
        background_subagents: Default::default(),
    };

    if !engine_config.working_dir.is_dir() {
        anyhow::bail!(
            "working directory `{}` does not exist — pass --working-dir <path>",
            engine_config.working_dir.display()
        );
    }

    let mut providers: HashMap<String, Arc<dyn ModelProvider>> = HashMap::new();
    // No-key startup: if the chosen provider can't start (usually a missing
    // key) but another provider has a key, switch instead of dying. The
    // user sees a note; a provider-specific default model follows the
    // switch unless the user pinned a model explicitly.
    let (provider_id, main_provider) = match build_provider(&provider_id) {
        Ok(provider) => (provider_id, provider),
        Err(err) => match detect_available_provider(&provider_id) {
            Some(alt) => {
                let note =
                    format!("provider `{provider_id}` unavailable ({err:#}) — using `{alt}`");
                eprintln!("note: {note}");
                tracing::warn!("{note}");
                let provider = build_provider(&alt)?;
                (alt, provider)
            }
            None => return Err(err),
        },
    };
    // The engine's fallback target must follow the switch — a stale
    // default_provider names a provider that never got registered.
    engine_config.default_provider = provider_id.clone();
    let model_explicit = cli.model.is_some() || cfg.default_model.is_some();
    if !model_explicit {
        if let Some(m) = default_model_for_provider(&provider_id) {
            engine_config.default_model = m;
        }
    }
    providers.insert(provider_id.clone(), main_provider);

    // Register every OTHER provider that can build from the environment
    // (API keys, saved sign-ins). /model switches the engine's provider
    // along with the model — an unregistered provider made the switch a
    // hard error ("no provider registered under `chatgpt`"). The list is
    // every factory-buildable id except "mock": a set GEMINI_API_KEY or
    // GROQ_API_KEY must not leave `/model gemini-2.5-pro` dead on arrival.
    for extra_id in [
        "anthropic",
        "openai-compat",
        "chatgpt",
        "zai",
        "google",
        "groq",
        "cerebras",
        "mistral",
        "nvidia",
        "openrouter",
        "github-models",
    ] {
        if !providers.contains_key(extra_id) {
            if let Ok(built) = build_provider(extra_id) {
                providers.insert(extra_id.to_string(), built);
            }
        }
    }

    let store = Arc::new(JsonlSessionStore::new(&sessions_dir));
    let mut registry = default_registry_with_memory(&memory_root);
    if let Some(verify) = &cfg.verify {
        registry.register(Arc::new(VerifyTool::new(&verify.command)));
    }
    // Phase 4: read-only subagent for repo-wide questions; skills load
    // on demand — the tool only registers when the repo HAS skills.
    registry.register(Arc::new(modron_tools::ExploreTool));
    // Phase 5c A2: the delegate tool itself (2026-11-11: it was never
    // registered — only its sidekicks below were, leaving the model
    // unable to spawn subagents and answering "no" when asked). Essential
    // tier so the model sees it in the schema and auto-delegates
    // (oh-my-pi pattern): background subagents via `delegate_status`,
    // isolated worktrees via `merge_worktree`.
    registry.register(Arc::new(modron_tools::DelegateTool));
    // The standing plan (2026-11-11): measurable subtasks + statuses,
    // re-injected every turn so long goals stay on track.
    registry.register(Arc::new(modron_tools::PlanTool));
    registry.register(Arc::new(modron_tools::DelegateStatusTool));
    registry.register(Arc::new(modron_tools::MergeWorktreeTool));
    // Built-in methodology skills ship with the binary, so the tool is
    // always registered (repo skills still add/override on top).
    registry.register(Arc::new(modron_tools::SkillTool::for_working_dir(
        &working_dir,
    )));
    // Tool tiering: only ESSENTIAL_TOOLS stay in the prompt schema; the
    // rest (explore, delegate_status, merge_worktree, memory pair,
    // read_minified) are on-demand and discoverable via tool_search.
    modron_tools::tier_default_tools(&mut registry);
    for reg in mcp {
        registry.register(Arc::new(modron_mcp::McpTool::new(
            Arc::clone(&reg.handle),
            &reg.server,
            reg.info.clone(),
        )));
        // Phase 13 dynamic discovery: MCP tools stay callable but leave the
        // system prompt — the model finds them via tool_search.
        registry.mark_on_demand(&reg.info.name);
    }
    // Personas (Phase 5c A2): catalog-resolve providers so delegates can
    // force (model, provider) pairs without a catalog dependency in core.
    let mut personas = std::collections::BTreeMap::new();
    for (name, p) in cfg.agents.clone().unwrap_or_default() {
        let provider = match (p.provider.clone(), p.model.clone()) {
            (Some(provider), _) => Some(provider),
            (None, Some(model)) => modron_router::lookup(&model).map(|info| info.provider.clone()),
            (None, None) => None,
        };
        personas.insert(
            name,
            modron_core::PersonaSpec {
                system: p.system,
                model: p.model,
                provider,
                allow: p.allow.unwrap_or_default(),
                deny: p.deny.unwrap_or_default(),
            },
        );
    }
    engine_config.personas = personas;
    // Build the router FIRST even when pinning: its config errors (unknown
    // mode, bad overrides) must surface regardless of the provider choice.
    // Tiers derive from the providers that actually built (keys/sign-ins).
    let authed: Vec<String> = providers.keys().cloned().collect();
    let (router, mut tier_fallbacks) =
        build_selector(cfg, persona.as_ref().and_then(|p| p.model.clone()), &authed)?;
    let selector = match &cli.provider {
        // An explicitly chosen provider PINS every turn there — the
        // router would otherwise override it with its own cheap/strong
        // tiers (a `--provider mock` run was silently routed to
        // anthropic the moment anthropic became buildable). When the
        // no-key fallback switched away from the requested provider, the
        // pin must NOT keep naming the unbuildable one — the authed-tier
        // router takes over instead.
        Some(choice) if choice.id() == provider_id => {
            // An explicit --provider pin is an explicit choice: the tier
            // fallback must never silently swap it.
            tier_fallbacks = None;
            Arc::new(modron_core::testing::StaticSelector::new(
                &engine_config.default_model,
                choice.id(),
            )) as Arc<dyn modron_core::ModelSelector>
        }
        _ => router,
    };
    engine_config.tier_fallbacks = tier_fallbacks;
    let engine = Engine {
        selector,
        providers,
        tools: Arc::new(registry),
        store: Arc::clone(&store) as Arc<dyn SessionStore>,
        config: engine_config,
    };
    Ok(Agent {
        engine,
        store,
        resume: cli.resume.flatten(),
    })
}

/// Interactive approver (Phase-1c I2): prompt on the terminal via /dev/tty
/// so piped stdin keeps working. `y` allows once, `a` allows for the rest of
/// the run, anything else denies. Returns `None` when stdin is not a
/// terminal — Ask mode then keeps its block-and-explain behavior.
fn tty_approver() -> Option<Arc<modron_core::Approver>> {
    use std::io::IsTerminal as _;
    if !std::io::stdin().is_terminal() {
        return None;
    }
    let rest_approved = Arc::new(std::sync::atomic::AtomicBool::new(false));
    use std::sync::atomic::Ordering;
    Some(Arc::new(move |call: &ToolCall| {
        if rest_approved.load(Ordering::Relaxed) {
            return true;
        }
        // The whole tty round-trip is blocking I/O inside an async context:
        // block_in_place frees the runtime to keep other workers (streaming
        // deltas, background subagents) alive while the human decides — the
        // same pattern the TUI approver uses.
        tokio::task::block_in_place(|| {
            let Ok(mut tty) = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/tty")
            else {
                return false;
            };
            use std::io::Write as _;
            let args = short(&call.arguments.to_string(), 90);
            let _ = write!(tty, "\r\nmodron: allow {} {}? [y/N/a] ", call.name, args);
            let _ = tty.flush();
            let mut line = String::new();
            let ok = std::io::BufRead::read_line(
                &mut std::io::BufReader::new(tty.try_clone().unwrap_or_else(|_| {
                    // A failed clone ends the read; treat as denial.
                    std::fs::File::open("/dev/null").expect("devnull")
                })),
                &mut line,
            )
            .is_ok();
            if !ok {
                return false;
            }
            match line.trim() {
                "a" | "A" => {
                    rest_approved.store(true, Ordering::Relaxed);
                    true
                }
                "y" | "Y" | "yes" => true,
                _ => false,
            }
        })
    }))
}

/// Chat-mode delta sink (Phase-1c I1 consumer): text prints live; `Done`
/// ends the line; reasoning stays out of v1 output.
fn stdout_delta_sink() -> Arc<modron_core::DeltaSink> {
    Arc::new(move |delta: StreamDelta| match delta {
        StreamDelta::Text(text) => {
            print!("{text}");
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }
        StreamDelta::Reasoning(_) => {}
        StreamDelta::ToolCallReady(_) => {}
        StreamDelta::Done => println!(),
    })
}

/// Construct one provider by id (mock/demo or env-backed), shared by the
/// agent builder and the multi-provider bench.
pub(crate) fn build_provider_shim(pid: &str) -> anyhow::Result<Arc<dyn ModelProvider>> {
    Ok(match pid {
        "mock" => Arc::new(modron_core::testing::rich_demo_provider()),
        other => Arc::new(RetryingProvider::new(provider_from_env(other).map_err(
            |err| {
                anyhow::anyhow!(err.to_string()).context(format!("cannot start provider `{other}`"))
            },
        )?)),
    })
}

/// Which surfaces a run drives (replaces the two-bool `for_mode(bool, bool)`
/// whose six of eight call sites passed equal bools - the 2026-11-12 audit).
#[derive(Clone, Copy)]
pub(crate) struct RunMode {
    interactive: bool,
    stream: bool,
}

impl RunMode {
    /// The TUI/ACP path: no stdout stream (the UI renders), tty approver.
    pub(crate) const INTERACTIVE: Self = Self {
        interactive: true,
        stream: false,
    };
    /// Headless one-shots: no approver, no stream.
    pub(crate) const HEADLESS: Self = Self {
        interactive: false,
        stream: false,
    };
    /// Headless WITH streaming (serve/daemon/direct runs print deltas).
    pub(crate) const STREAM: Self = Self {
        interactive: false,
        stream: true,
    };
    /// The eval/bench path: tty approver AND stream.
    pub(crate) const INTERACTIVE_STREAM: Self = Self {
        interactive: true,
        stream: true,
    };
}

/// Load one attached file as an [`Attachment`] (Phase 7): images
/// base64-encoded by extension; text files inlined; PDFs via `pdftotext`
/// when installed (skipped with an actionable error otherwise).
pub(crate) fn load_attachment(path: &Path) -> anyhow::Result<modron_core::Attachment> {
    let bytes = std::fs::read(path)
        .map_err(|e| anyhow::anyhow!("cannot read attachment {}: {e}", path.display()))?;
    let media_type = match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    };
    if let Some(media_type) = media_type {
        // Sanity: image magic bytes.
        let looks_image = bytes.starts_with(&[0x89, b'P', b'N', b'G'])
            || bytes.starts_with(&[0xFF, 0xD8])
            || bytes.starts_with(b"GIF8")
            || bytes.len() > 4 && &bytes[..4] == b"RIFF";
        if !looks_image {
            anyhow::bail!(
                "{} has an image extension but is not an image",
                path.display()
            );
        }
        return Ok(modron_core::Attachment::Image {
            media_type: media_type.to_string(),
            data: modron_core::b64::encode(&bytes),
        });
    }
    // PDF via pdftotext (Claude Code's system-tool approach).
    if path.extension().and_then(|e| e.to_str()) == Some("pdf") {
        let output = std::process::Command::new("pdftotext")
            .arg(path)
            .arg("-")
            .output()
            .map_err(|_| {
                anyhow::anyhow!(
                    "pdftotext is not installed — brew install poppler (or attach text files)"
                )
            })?;
        if !output.status.success() {
            anyhow::bail!("pdftotext failed on {}", path.display());
        }
        return Ok(modron_core::Attachment::Document {
            name: path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string(),
            text: String::from_utf8_lossy(&output.stdout).to_string(),
        });
    }
    // Everything else: UTF-8 text document.
    let text = String::from_utf8(bytes).map_err(|_| {
        anyhow::anyhow!(
            "{} is neither an image nor UTF-8 text (supported: png/jpg/gif/webp, text, pdf)",
            path.display()
        )
    })?;
    Ok(modron_core::Attachment::Document {
        name: path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string(),
        text,
    })
}

/// `modron doctor` (Phase 8): diagnose the setup — API keys, config,
/// git, PDF support, and the terminal. Every check prints ✓/✗ with an
/// actionable fix; the exit code reflects overall health.
fn run_doctor(_cfg: &config::Config) -> anyhow::Result<()> {
    let mut healthy = true;

    println!("modron {} — environment check", env!("CARGO_PKG_VERSION"));
    println!();

    // API keys (by provider).
    let providers = [
        (
            "anthropic",
            "ANTHROPIC_API_KEY",
            "https://console.anthropic.com",
        ),
        (
            "openai-compat",
            "OPENAI_API_KEY",
            "https://platform.openai.com",
        ),
        ("zai", "ZAI_API_KEY", "https://z.ai"),
        ("groq", "GROQ_API_KEY", "https://console.groq.com/keys"),
        (
            "google",
            "GEMINI_API_KEY",
            "https://aistudio.google.com/apikey",
        ),
        ("cerebras", "CEREBRAS_API_KEY", "https://cloud.cerebras.ai"),
        ("mistral", "MISTRAL_API_KEY", "https://console.mistral.ai"),
        (
            "openrouter",
            "OPENROUTER_API_KEY",
            "https://openrouter.ai/keys",
        ),
        (
            "github-models",
            "GITHUB_MODELS_TOKEN",
            "https://github.com/settings/tokens",
        ),
        ("nvidia", "NVIDIA_API_KEY", "https://build.nvidia.com"),
    ];
    println!("providers:");
    for (name, env_key, hint) in providers {
        match std::env::var(env_key) {
            Ok(v) if !v.is_empty() => println!("  ✓ {name} ({env_key} set)"),
            _ => println!("  · {name} — unset ({hint})"),
        }
    }

    // Config file.
    println!("\nconfig:");
    match config::config_path() {
        Some(path) if path.exists() => {
            // Try parsing it via load_from.
            let loaded = config::load_from(&path, |_| None);
            println!("  ✓ {} (parses)", path.display());
            if let Some(default) = &loaded.default_provider {
                println!("    default provider: {default}");
            }
            if let Some(model) = &loaded.default_model {
                println!("    default model: {model}");
            }
        }
        Some(path) => println!(
            "  · no config file at {} (optional — defaults apply)",
            path.display()
        ),
        None => println!("  · no home directory found; config unavailable"),
    }

    // Git (worktree isolation + checkpoints).
    println!("\ntools:");
    let git = std::process::Command::new("git").arg("--version").output();
    match git {
        Ok(o) if o.status.success() => {
            println!("  ✓ git ({})", String::from_utf8_lossy(&o.stdout).trim())
        }
        _ => {
            healthy = false;
            println!(
                "  ✗ git not found — worktree isolation and diffs unavailable (brew install git)"
            );
        }
    }
    match std::process::Command::new("pdftotext").arg("-v").output() {
        Ok(_) => println!("  ✓ pdftotext (PDF attachments work)"),
        _ => println!("  · pdftotext not found — PDF attachments skipped (brew install poppler)"),
    }

    // Sessions + memory dirs.
    println!("\nstorage:");
    if let Some(home) = dirs::home_dir() {
        let sessions = home.join(".modron").join("sessions");
        let count = std::fs::read_dir(&sessions).map(|d| d.count()).unwrap_or(0);
        println!("  sessions: {} in {}", count, sessions.display());
        let memory = home.join(".modron").join("memory");
        let mem_count = std::fs::read_dir(&memory).map(|d| d.count()).unwrap_or(0);
        println!("  memory workspaces: {mem_count}");
    }

    // Terminal.
    println!("\nterminal:");
    use std::io::IsTerminal as _;
    println!(
        "  stdout is {}: TUI {}",
        if std::io::stdout().is_terminal() {
            "a tty"
        } else {
            "piped"
        },
        if std::io::stdout().is_terminal() {
            "available"
        } else {
            "unavailable (use chat or one-shot)"
        }
    );

    println!();
    if healthy {
        println!("all critical checks passed — run: modron \"your task\"");
    } else {
        println!("some checks failed — see the ✗ lines above");
    }
    Ok(())
}

/// `modron pair [<name>|list|revoke <name>]` (Phase 3): mint, list, and
/// revoke the device tokens remote clients present on `modron serve`'s
/// non-loopback WebSocket endpoint.
fn run_pair(name: Option<String>, action: Option<PairAction>) -> anyhow::Result<()> {
    let dir = modron_server::default_clients_dir()?;
    match action {
        Some(PairAction::List) => {
            let devices = modron_server::list(&dir)?;
            if devices.is_empty() {
                println!("no paired devices — mint one with `modron pair <name>`");
                return Ok(());
            }
            for device in devices {
                println!("{}  (paired {})", device.name, device.created_at);
            }
        }
        Some(PairAction::Revoke { name }) => {
            if modron_server::revoke(&dir, &name)? {
                println!("revoked {name}");
            } else {
                println!("no paired device named {name}");
            }
        }
        None => {
            // Bare `modron pair [name]`: mint + print. The token is shown
            // exactly once; the file stores it for verification.
            let device = modron_server::grant(&dir, name.as_deref().unwrap_or("default"))?;
            let path = dir.join(format!("{}.token", device.name));
            println!("paired device `{}` — token:", device.name);
            println!("{}", device.token);
            println!("stored at {}", path.display());
            println!(
                "send it in the hello frame of every remote client (loopback and \
                 unix-socket connections don't need it)"
            );
        }
    }
    Ok(())
}

/// `modron daemon start|status|stop` (Phase 9): the persistent engine.
async fn run_daemon(action: DaemonAction, cli: &Cli, cfg: &config::Config) -> anyhow::Result<()> {
    match action {
        DaemonAction::Status => {
            if modron_daemon::is_running() {
                let pid = std::fs::read_to_string(modron_daemon::pid_file()?)
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                println!(
                    "daemon running (pid {pid}) — socket at {}",
                    modron_daemon::socket_path()?.display()
                );
            } else {
                println!("daemon not running — start with `modron daemon start`");
            }
            Ok(())
        }
        DaemonAction::Stop => {
            if !modron_daemon::is_running() {
                println!("daemon not running");
                return Ok(());
            }
            // Connect and send stop.
            use tokio::io::AsyncWriteExt;
            if let Some(mut stream) = modron_daemon::connect().await? {
                let msg = "{\"id\":1,\"method\":\"stop\",\"params\":{}}\n";
                stream.write_all(msg.as_bytes()).await?;
            }
            // Bounded verification poll: report what actually happened, not
            // an unconditional success.
            for _ in 0..20 {
                if !modron_daemon::is_running() {
                    println!("daemon stopped");
                    return Ok(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            anyhow::bail!("daemon did not stop within 2s — check `daemon status`");
        }
        DaemonAction::Start => {
            if modron_daemon::is_running() {
                println!("daemon already running — `modron daemon status` for details");
                return Ok(());
            }
            // Build the engine with the current config.
            let mcp = spawn_mcp_servers(cfg).await?;
            let agent = build_agent_with(
                cli,
                cfg,
                false,
                AgentSinks::for_mode(RunMode::HEADLESS),
                &mcp,
            )?;
            let daemon = modron_daemon::Daemon::new(agent.engine);
            println!("starting daemon (ctrl+c to stop)…");
            daemon.run().await?;
            Ok(())
        }
    }
}

/// Prepared MCP registration: one tool on one live server.
pub struct McpRegistration {
    pub server: String,
    pub handle: Arc<tokio::sync::Mutex<modron_mcp::McpServerHandle>>,
    pub info: modron_mcp::McpToolInfo,
}

/// Spawn every configured MCP server and collect its tools (Phase 5a).
/// Per-server schema cost is enforced here — a server whose tools would
/// fatten the prompt past its budget is refused with an actionable error.
async fn spawn_mcp_servers(cfg: &config::Config) -> anyhow::Result<Vec<McpRegistration>> {
    let Some(mcp) = &cfg.mcp else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for (name, server) in &mcp.servers {
        let env: Vec<(String, String)> =
            server.env.clone().unwrap_or_default().into_iter().collect();
        let args = server.args.clone().unwrap_or_default();
        let (handle, tools) =
            modron_mcp::McpServerHandle::spawn(name, &server.command, &args, &env).await?;
        let total: u64 = tools.iter().map(|t| t.schema_tokens).sum();
        let cap = server.max_schema_tokens.unwrap_or(20_000);
        if total > cap {
            return Err(anyhow::anyhow!(
                "MCP server `{name}` tool schemas cost {total} est. tokens (cap {cap}) — \
                 drop tools from the server or raise max_schema_tokens"
            ));
        }
        tracing::info!(server = %name, tools = tools.len(), schema_tokens = total,
            "mcp server ready");
        for info in tools {
            out.push(McpRegistration {
                server: name.clone(),
                handle: Arc::clone(&handle),
                info,
            });
        }
    }
    Ok(out)
}

/// The selector plus the auth-fallback candidates derived alongside it
/// (`None` whenever the tiers are explicit: `[router]` overrides, a
/// persona's forced model, or a `--provider` pin).
type SelectorWithFallbacks = (
    Arc<dyn modron_core::ModelSelector>,
    Option<Arc<modron_core::TierFallbacks>>,
);

/// Build the active selector from `[router]` config (Phase-3a R1).
/// Default mode is `learned` (corpus centroids + heuristic fallback — the
/// fallback preserves budget/overflow rules, so learned is a strict
/// refinement of the rules).
fn build_selector(
    cfg: &config::Config,
    persona_model: Option<String>,
    authed_providers: &[String],
) -> anyhow::Result<SelectorWithFallbacks> {
    use modron_router::{learned::LearnedRouter, HeuristicSelector};
    let router_cfg = cfg.router.clone().unwrap_or_default();
    // Default tiers derive from the AUTHED models (what the user's keys
    // and sign-ins can actually reach); explicit [router] config wins.
    let derived = modron_router::derive_tiers(authed_providers);
    let mut tier_fallbacks: Option<Arc<modron_core::TierFallbacks>> = None;
    if router_cfg.cheap_model.is_none() && router_cfg.strong_model.is_none() {
        // Auto-routing mode: derive the ORDERED fallback candidates too,
        // so a dead provider token cannot take a whole tier down (the
        // engine retries once on the next candidate from another
        // provider). A persona's forced model is an explicit choice —
        // never silently swapped.
        if persona_model.is_none() {
            if let Some(candidates) = modron_router::derive_tier_candidates(authed_providers) {
                let pair_of = |model: &str| {
                    modron_router::lookup(model)
                        .map(|info| (model.to_string(), info.provider.clone()))
                };
                let list = |names: &[String]| -> Vec<(String, String)> {
                    names.iter().filter_map(|n| pair_of(n)).collect()
                };
                let cheap = list(&candidates.cheap);
                let strong = list(&candidates.strong);
                if cheap.len() > 1 || strong.len() > 1 {
                    // The engine skips same-provider candidates itself, so
                    // the full chain is what an operator wants to see.
                    let alts = |v: &[(String, String)]| -> String {
                        v[1..]
                            .iter()
                            .map(|(m, p)| format!("{m} ({p})"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    };
                    eprintln!(
                        "note: auto-routing from your signed-in providers — cheap: {}, strong: {} (auth fallbacks — cheap: {}; strong: {}; override in [router])",
                        cheap[0].0,
                        strong[0].0,
                        alts(&cheap),
                        alts(&strong)
                    );
                    tier_fallbacks = Some(Arc::new(modron_core::TierFallbacks { cheap, strong }));
                }
            }
        }
        // One note per startup: the enriched line above when fallbacks
        // exist, the plain line otherwise.
        if tier_fallbacks.is_none() {
            if let Some((cheap, strong)) = &derived {
                eprintln!(
                    "note: auto-routing from your signed-in providers — cheap: {cheap}, strong: {strong} (override in [router])"
                );
            }
        }
    }
    let (default_cheap, default_strong) = derived.unwrap_or_else(|| {
        // Nothing authed and cataloged: prefer the active provider's own
        // catalog default over the classic anthropic pair — the fallback
        // must never name a provider that is not registered (mock runs pin
        // the provider explicitly anyway).
        authed_providers
            .iter()
            .find(|p| *p != "mock")
            .and_then(|p| default_model_for_provider(p))
            .map(|m| (m.clone(), m))
            .unwrap_or_else(|| {
                (
                    "claude-haiku-5-5".to_string(),
                    "claude-sonnet-5-5".to_string(),
                )
            })
    });
    let cheap = router_cfg.cheap_model.clone().unwrap_or(default_cheap);
    let strong = router_cfg.strong_model.clone().unwrap_or(default_strong);
    let provider_of = |model: &str| {
        modron_router::lookup(model)
            .map(|info| info.provider.clone())
            .unwrap_or_else(|| "anthropic".to_string())
    };
    let overrides: std::collections::HashMap<modron_core::ActionKind, String> = router_cfg
        .actions
        .clone()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(action, model)| match action.as_str() {
            "chat" => Some((modron_core::ActionKind::Chat, model)),
            "search" => Some((modron_core::ActionKind::Search, model)),
            "summarize" => Some((modron_core::ActionKind::Summarize, model)),
            "plan" => Some((modron_core::ActionKind::Plan, model)),
            "edit" => Some((modron_core::ActionKind::Edit, model)),
            other => {
                tracing::warn!(action = other, "unknown [router.actions] key — ignored");
                None
            }
        })
        .collect();
    let heuristic = HeuristicSelector::new(
        (cheap.clone(), provider_of(&cheap)),
        (strong.clone(), provider_of(&strong)),
    )
    .with_overrides(overrides)
    .with_forced_model(persona_model);
    match router_cfg.mode.as_deref() {
        None | Some("learned") => Ok((Arc::new(LearnedRouter::new(heuristic)), tier_fallbacks)),
        Some("heuristic") => Ok((Arc::new(heuristic), tier_fallbacks)),
        Some(other) => Err(anyhow::anyhow!(
            "unknown router mode `{other}` — use \"learned\" or \"heuristic\"",
        )),
    }
}

/// `modron sessions vacuum --older-than-days N` (Phase 6a): delete sessions
/// whose last update is older than N days. `--dry-run` lists what would go.
/// The `.rewind` markers die with their sessions.
async fn run_sessions_vacuum(
    sessions_dir: &Path,
    older_than_days: u64,
    dry_run: bool,
) -> anyhow::Result<()> {
    let cutoff = chrono::Utc::now() - chrono::Duration::days(older_than_days as i64);
    let store = JsonlSessionStore::new(sessions_dir);
    let sessions = store
        .list()
        .await
        .map_err(|err| anyhow::anyhow!("cannot list sessions: {err}"))?;
    let expired: Vec<_> = sessions
        .into_iter()
        .filter(|s| s.updated_at < cutoff)
        .collect();
    if expired.is_empty() {
        println!("nothing to vacuum (no sessions older than {older_than_days} days)");
        return Ok(());
    }
    for meta in &expired {
        println!(
            "{}  {}  (last update {})",
            if dry_run { "would delete" } else { "deleting" },
            meta.id,
            meta.updated_at.format("%Y-%m-%d")
        );
    }
    if dry_run {
        println!("dry run — nothing deleted");
        return Ok(());
    }
    let mut deleted = 0usize;
    for meta in &expired {
        if store.delete(meta.id).await.unwrap_or(false) {
            deleted += 1;
        }
    }
    println!("vacuumed {deleted} sessions");
    Ok(())
}

/// `modron sessions restart <id>` (Phase 6a): same session id, fresh
/// context. The history stays on disk (append-only log) but the resume
/// marker moves to the log's end, so the next `--resume` starts clean.
async fn run_sessions_restart(sessions_dir: &Path, id: SessionId) -> anyhow::Result<()> {
    let store = JsonlSessionStore::new(sessions_dir);
    let events = store
        .load(id)
        .await
        .map_err(|err| anyhow::anyhow!("cannot load session: {err}"))?;
    if events.is_empty() {
        anyhow::bail!("session {id} has no events — nothing to restart");
    }
    let marker = sessions_dir.join(format!("{id}.rewind"));
    std::fs::write(&marker, events.len().to_string())
        .map_err(|err| anyhow::anyhow!("cannot write restart marker: {err}"))?;
    println!(
        "session {id} restarted: {} events kept on disk, context starts fresh on the next --resume",
        events.len()
    );
    Ok(())
}

/// `modron mcp info <name>` (Phase 5e): spawn one configured server and
/// print its tools (with schema costs), resources, and prompts.
async fn run_mcp_info(cfg: &config::Config, name: String) -> anyhow::Result<()> {
    let Some(mcp) = &cfg.mcp else {
        anyhow::bail!("no [mcp.servers] configured");
    };
    let Some(server) = mcp.servers.get(&name) else {
        let available: Vec<&str> = mcp.servers.keys().map(|s| s.as_str()).collect();
        anyhow::bail!(
            "unknown MCP server `{name}` — configured: {}",
            if available.is_empty() {
                "(none)".to_string()
            } else {
                available.join(", ")
            }
        );
    };
    let env: Vec<(String, String)> = server.env.clone().unwrap_or_default().into_iter().collect();
    let args = server.args.clone().unwrap_or_default();
    let (handle, tools) =
        modron_mcp::McpServerHandle::spawn(&name, &server.command, &args, &env).await?;
    let mut handle = handle.lock().await;

    let total: u64 = tools.iter().map(|t| t.schema_tokens).sum();
    println!(
        "server {name} — {} tools, {total} est. schema tokens",
        tools.len()
    );
    for tool in &tools {
        println!(
            "  ● {:<30} {:>5} tok — {}",
            tool.name, tool.schema_tokens, tool.description
        );
    }
    match handle.list_resources().await {
        Ok(resources) if !resources.is_empty() => {
            println!("resources:");
            for r in &resources {
                println!("  {} — {}", r.uri, r.description);
            }
        }
        _ => println!("resources: none"),
    }
    match handle.list_prompts().await {
        Ok(prompts) if !prompts.is_empty() => {
            println!("prompts:");
            for p in &prompts {
                println!("  {} — {}", p.name, p.description);
            }
        }
        _ => println!("prompts: none"),
    }
    Ok(())
}

/// `modron mcp read <server> <uri>`: read one resource's contents
/// (2026-11-12 audit: `read_resource` existed and was tested, but no
/// surface could reach it).
async fn run_mcp_read(cfg: &config::Config, name: String, uri: String) -> anyhow::Result<()> {
    let handle = mcp_handle(cfg, &name).await?;
    let mut handle = handle.lock().await;
    let text = handle.read_resource(&uri).await?;
    print!("{text}");
    if !text.ends_with('\n') {
        println!();
    }
    Ok(())
}

/// Shared MCP plumbing: validate the configured server and spawn it.
async fn mcp_handle(
    cfg: &config::Config,
    name: &str,
) -> anyhow::Result<Arc<tokio::sync::Mutex<modron_mcp::McpServerHandle>>> {
    let Some(mcp) = &cfg.mcp else {
        anyhow::bail!("no [mcp.servers] configured");
    };
    let Some(server) = mcp.servers.get(name) else {
        let available: Vec<&str> = mcp.servers.keys().map(|s| s.as_str()).collect();
        anyhow::bail!(
            "unknown MCP server `{name}` — configured: {}",
            if available.is_empty() {
                "(none)".to_string()
            } else {
                available.join(", ")
            }
        );
    };
    let env: Vec<(String, String)> = server.env.clone().unwrap_or_default().into_iter().collect();
    let args = server.args.clone().unwrap_or_default();
    let (handle, _tools) =
        modron_mcp::McpServerHandle::spawn(name, &server.command, &args, &env).await?;
    Ok(handle)
}

/// `modron traces <session>`: tabulate the session's TurnTrace rows
/// (2026-11-12 audit: the harness-observability records were persisted
/// every turn with no reader anywhere).
async fn run_traces(sessions_dir: &std::path::Path, session: &str) -> anyhow::Result<()> {
    let id = uuid::Uuid::parse_str(session.trim())
        .map_err(|_| anyhow::anyhow!("session must be a uuid (see `modron sessions list`)"))?;
    let store = modron_store::JsonlSessionStore::new(sessions_dir);
    let events = store.load(id).await?;
    let mut rows = Vec::new();
    for event in &events {
        if let modron_core::SessionEvent::TurnTrace {
            turn,
            provider,
            model,
            prompt_hash,
            latency_ms,
            cost_usd,
            usage,
            verifier,
            failure_label,
            ..
        } = event
        {
            rows.push((
                *turn,
                provider.clone(),
                model.clone(),
                prompt_hash.clone(),
                *latency_ms,
                *cost_usd,
                *usage,
                verifier.clone(),
                failure_label.clone(),
            ));
        }
    }
    if rows.is_empty() {
        println!("no TurnTrace rows in session {id}");
        return Ok(());
    }
    println!(
        "  turn  provider   model                  latency    in+out      cost  verify   stop"
    );
    let mut total_cost = 0.0;
    for (turn, provider, model, hash, latency, cost, usage, verifier, stop) in &rows {
        let tok = usage
            .map(|u| u.input_tokens + u.output_tokens)
            .map(|t| t.to_string())
            .unwrap_or_else(|| "-".into());
        let lat = latency
            .map(|l| format!("{l}ms"))
            .unwrap_or_else(|| "-".into());
        let cost_s = cost
            .map(|c| {
                total_cost += c;
                format!("${:.4}", c)
            })
            .unwrap_or_else(|| "-".into());
        println!(
            "{:>5}  {:<10} {:<22} {:>9} {:>9} {:>9}  {:<8} {}",
            turn,
            truncate(provider, 10),
            truncate(model, 22),
            lat,
            tok,
            cost_s,
            verifier.as_deref().unwrap_or("-"),
            stop.as_deref().unwrap_or("-")
        );
        if let Some(h) = hash {
            println!("      prompt_hash: {}", &h[..h.len().min(16)]);
        }
    }
    println!("{} turn(s) · total cost ${:.4}", rows.len(), total_cost);
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

/// The curated FREE model list (Phase 5f, OpenCode-style comparison):
/// models verified to complete without balance on the user's provider.
/// Z.ai's flash tier is free; extend here (or pass --models) as more
/// free tiers get verified.
pub fn curated_free_models() -> Vec<&'static str> {
    vec!["glm-4.7-flash", "glm-4.5-flash"]
}

/// The fixed benchmark suite (Phase 5e): read-only work, mutation, and
/// multi-step flows. Deterministic and provider-agnostic.
pub fn bench_suite() -> Vec<&'static str> {
    vec![
        "list the rust files in this repo",
        "read the first 20 lines of Cargo.toml and summarize them",
        "create a file named bench-marker.txt containing the word ok, then read it back",
        "find where the budget governor is implemented and report the file",
        "edit bench-marker.txt to say done instead of ok, then verify by reading it",
        "count how many test functions exist in crates/modron-core/src/engine.rs",
    ]
}

/// One completed benchmark task.
#[derive(serde::Serialize)]
struct BenchRecord {
    provider: String,
    model: String,
    task: String,
    completed: bool,
    turns: u32,
    tool_calls: u32,
    tokens: u64,
    cached_tokens: u64,
    duration_ms: u128,
}

/// One row of the `modron bench` comparison table (and, with `--json`, of
/// the machine-readable output). `completed` is the raw count; the fraction
/// is derived against the suite size at write time.
#[derive(serde::Serialize)]
struct BenchComparisonRow {
    model: String,
    completed: usize,
    total_tasks: usize,
    tokens_per_task: f64,
    ms_per_task: f64,
    /// 0.0-100.0; `None` when the model consumed no tokens at all.
    cache_pct: Option<f64>,
}

/// `modron bench` (Phase 5f): run the suite across a model list, print the
/// per-model comparison, and persist every task record to
/// `~/.modron/bench/<timestamp>.jsonl` — the accumulating baseline file
/// PLAN's cost tracking reads from. `--json <path>` additionally writes the
/// per-model comparison rows as a JSON array.
async fn run_bench(
    cli: &Cli,
    cfg: &config::Config,
    models_arg: Option<&str>,
    list: bool,
    delay_ms: Option<u64>,
    json_path: Option<&Path>,
) -> anyhow::Result<()> {
    if list {
        for model in curated_free_models() {
            println!("{model}");
        }
        return Ok(());
    }
    let models: Vec<String> = match models_arg {
        // Empty CSV segments (`"a,,b"`, `","`) must not become empty-string
        // model names that only fail later inside provider calls.
        Some(csv) => csv
            .split(',')
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(String::from)
            .collect(),
        None => curated_free_models()
            .into_iter()
            .map(String::from)
            .collect(),
    };
    if models.is_empty() {
        anyhow::bail!("--models parsed to an empty list");
    }
    let suite = bench_suite();
    let mcp = spawn_mcp_servers(cfg).await?;

    // Multi-provider support: each model's provider comes from the catalog;
    // the union of providers is built once. Free tiers all speak
    // OpenAI-compatible HTTP (Phase 5g factory), so one client shape covers
    // every row of the comparison table.
    let mut providers: HashMap<String, Arc<dyn ModelProvider>> = HashMap::new();
    let default_provider = cli
        .provider
        .map(|p| p.id().to_string())
        .or_else(|| cfg.default_provider.clone())
        .unwrap_or_else(|| "anthropic".to_string());
    providers.insert(
        default_provider.clone(),
        build_provider_shim(&default_provider)?,
    );
    let mut skipped: Vec<(String, String)> = Vec::new();
    for model in &models {
        let provider = modron_router::lookup(model)
            .map(|info| info.provider.clone())
            .unwrap_or_else(|| default_provider.clone());
        if !providers.contains_key(&provider) {
            match build_provider_shim(&provider) {
                Ok(built) => {
                    // Key by the CATALOG provider name (what the filter and
                    // fallback resolution use) — provider.id() reports
                    // "openai-compat" for zai-built instances.
                    providers.insert(provider.clone(), built);
                }
                Err(e) => {
                    // A missing key skips that model, not the whole bench.
                    tracing::warn!(model = %model, error = %e, "provider unavailable — skipping model");
                    skipped.push((model.clone(), e.to_string()));
                }
            }
        }
    }
    let models: Vec<String> = models
        .into_iter()
        .filter(|m| {
            let provider = modron_router::lookup(m)
                .map(|info| info.provider.clone())
                .unwrap_or_else(|| default_provider.clone());
            providers.contains_key(&provider)
        })
        .collect();
    for (model, reason) in &skipped {
        println!("✗ skipped {model}: {reason}");
    }
    println!(
        "bench: {} tasks × {} models, provider {}{}",
        suite.len(),
        models.len(),
        cli.provider.map(|p| p.id()).unwrap_or("zai (default)"),
        if mcp.is_empty() {
            String::new()
        } else {
            format!(", {} mcp servers", mcp.len())
        }
    );

    let mut all_records: Vec<BenchRecord> = Vec::new();
    let mut per_model: Vec<(String, usize, u64, u128)> = Vec::new();

    for model in &models {
        let mut model_cli = cli.clone();
        model_cli.model = Some(model.clone());
        let started = std::time::Instant::now();
        let mut completed = 0usize;
        let mut tokens = 0u64;
        let mut cached = 0u64;
        // The record's provider follows the catalog (multi-provider bench
        // runs must not persist every row as "zai").
        let model_provider = modron_router::lookup(model)
            .map(|i| i.provider.clone())
            .unwrap_or_else(|| {
                cli.provider
                    .map(|p| p.id().to_string())
                    .unwrap_or_else(|| "zai".to_string())
            });

        for task in &suite {
            let task_started = std::time::Instant::now();
            let agent = build_agent_with(
                &model_cli,
                cfg,
                false,
                AgentSinks::for_mode(RunMode::HEADLESS),
                &mcp,
            )?;
            // Re-wire the engine onto the multi-provider map (the builder
            // only registers the run's default provider).
            let mut engine = agent.engine;
            for (pid, provider) in &providers {
                engine.providers.insert(pid.clone(), Arc::clone(provider));
            }
            let record = match engine.run_task(task).await {
                Ok(summary) => {
                    let task_ok = !summary.answer.is_empty();
                    if task_ok {
                        completed += 1;
                    }
                    tokens += summary.usage.total();
                    cached += summary.usage.cached_input_tokens;
                    BenchRecord {
                        provider: model_provider.clone(),
                        model: model.clone(),
                        task: task.to_string(),
                        completed: task_ok,
                        turns: summary.turns,
                        tool_calls: summary.tool_calls,
                        tokens: summary.usage.total(),
                        cached_tokens: summary.usage.cached_input_tokens,
                        duration_ms: task_started.elapsed().as_millis(),
                    }
                }
                Err(e) => {
                    eprintln!("✗ [{model}] {task}: {e}");
                    BenchRecord {
                        provider: model_provider.clone(),
                        model: model.clone(),
                        task: task.to_string(),
                        completed: false,
                        turns: 0,
                        tool_calls: 0,
                        tokens: 0,
                        cached_tokens: 0,
                        duration_ms: task_started.elapsed().as_millis(),
                    }
                }
            };
            println!(
                "{:<3} [{:<18}] {:<50} {:>7} tok {:>4} turns {:>6} ms",
                if record.completed { "✓" } else { "✗" },
                model,
                short(task, 50),
                record.tokens,
                record.turns,
                record.duration_ms
            );
            if record.completed {
                completed += 0; // counted above
            }
            all_records.push(record);
            if let Some(ms) = delay_ms {
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            }
        }
        per_model.push((
            model.clone(),
            completed,
            tokens,
            started.elapsed().as_millis(),
        ));
        let _ = cached;
    }

    println!("\n=== comparison (est. cost from catalog) ===");
    println!(
        "{:<20} {:>9} {:>12} {:>12} {:>10} {:>10}",
        "model", "completed", "tokens/task", "ms/task", "cache %", "est. cost"
    );
    // One computed row serves both the printed table and `--json`, so the
    // two outputs can never drift apart.
    let rows: Vec<BenchComparisonRow> = per_model
        .iter()
        .map(|(model, completed, tokens, ms)| {
            let tasks = suite.len() as f64;
            let cached: u64 = all_records
                .iter()
                .filter(|r| &r.model == model)
                .map(|r| r.cached_tokens)
                .sum();
            let total_input: u64 = all_records
                .iter()
                .filter(|r| &r.model == model)
                .map(|r| r.tokens)
                .sum();
            BenchComparisonRow {
                model: model.clone(),
                completed: *completed,
                total_tasks: suite.len(),
                tokens_per_task: *tokens as f64 / tasks,
                ms_per_task: *ms as f64 / tasks,
                cache_pct: if total_input > 0 {
                    Some(100.0 * cached as f64 / total_input as f64)
                } else {
                    None
                },
            }
        })
        .collect();
    for (model, completed, tokens, ms) in &per_model {
        let tasks = suite.len() as f64;
        let price = modron_router::lookup(model)
            .map(|i| (i.input_per_m, i.output_per_m))
            .unwrap_or((0.0, 0.0));
        let tokens_per_task = *tokens as f64 / tasks;
        let cost_per_task =
            tokens_per_task / 1e6 * price.0 + (tokens_per_task * 0.12) / 1e6 * price.1;
        let cached: u64 = all_records
            .iter()
            .filter(|r| &r.model == model)
            .map(|r| r.cached_tokens)
            .sum();
        let total_input: u64 = all_records
            .iter()
            .filter(|r| &r.model == model)
            .map(|r| r.tokens)
            .sum();
        let cache_pct = if total_input > 0 {
            format!("{:.0}%", 100.0 * cached as f64 / total_input as f64)
        } else {
            "n/a".into()
        };
        println!(
            "{:<20} {:>9} {:>12.0} {:>12.0} {:>10} {:>10.4}",
            model,
            format!("{completed}/{}", suite.len()),
            tokens_per_task,
            *ms as f64 / tasks,
            cache_pct,
            cost_per_task * tasks
        );
    }

    // Machine-readable output: the same rows the table printed, as a JSON
    // array (Phase 5f follow-up — dashboards and diff scripts read this
    // instead of scraping the table).
    if let Some(path) = json_path {
        let body = serde_json::to_string_pretty(&rows)?;
        std::fs::write(path, body).map_err(|err| {
            anyhow::anyhow!("cannot write --json output to {}: {err}", path.display())
        })?;
        println!("json: {}", path.display());
    }

    // Persist the baseline (PLAN: measured, never claimed).
    if let Some(home) = dirs::home_dir() {
        let dir = home.join(".modron").join("bench");
        let _ = std::fs::create_dir_all(&dir);
        let stamp = chrono::Utc::now().to_rfc3339().replace([':', '+'], "-");
        let path = dir.join(format!("{stamp}.jsonl"));
        if let Ok(file) = std::fs::File::create(&path) {
            use std::io::Write as _;
            let mut file = std::io::BufWriter::new(file);
            for record in &all_records {
                if let Ok(line) = serde_json::to_string(record) {
                    let _ = writeln!(file, "{line}");
                }
            }
            println!("baseline: {}", path.display());
        }
    }

    // Bench hygiene: the suite's mutation tasks create files in the working
    // dir (bench-marker.txt) — remove the known artifacts so benches never
    // pollute the target repo.
    let mut artifact = cli.working_dir.clone();
    artifact.push("bench-marker.txt");
    if std::fs::remove_file(&artifact).is_ok() {
        tracing::debug!(path = %artifact.display(), "removed bench artifact");
    }
    Ok(())
}

/// `modron router eval` (Phase-3a R4): score the active selector against the\n/// `modron router eval` (Phase-3a R4): score the active selector against the
/// held-out eval set; report accuracy and the catalog-priced cost delta vs.
/// an always-strong baseline (synthetic 10k-in/1k-out task per example).
fn run_router_eval(cfg: &config::Config) -> anyhow::Result<()> {
    use modron_router::learned::run_eval;
    let authed: Vec<String> = ["anthropic", "openai-compat", "chatgpt", "zai", "google"]
        .iter()
        .filter(|id| build_provider_shim(id).is_ok())
        .map(|id| id.to_string())
        .collect();
    let (selector, _) = build_selector(cfg, None, &authed)?;
    let cases = run_eval(selector.as_ref());
    let router_cfg = cfg.router.clone().unwrap_or_default();
    let defaults = modron_router::derive_tiers(&authed)
        .unwrap_or_else(|| ("claude-haiku-5-5".into(), "claude-sonnet-5-5".into()));
    let cheap = router_cfg.cheap_model.clone().unwrap_or(defaults.0);
    let strong = router_cfg.strong_model.clone().unwrap_or(defaults.1);

    let correct = cases.iter().filter(|c| c.correct()).count();
    println!(
        "router eval — {} examples, accuracy {}/{} ({:.0}%)",
        cases.len(),
        correct,
        cases.len(),
        100.0 * correct as f64 / cases.len().max(1) as f64
    );
    for case in &cases {
        let mark = if case.correct() { "✓" } else { "✗" };
        println!(
            "{mark} {:<40} expected={:?} chose={:?}",
            short(&case.text, 40),
            case.expected,
            case.chosen
        );
        println!("    {}", short(&case.reason, 100));
    }

    // Cost delta per 1000 tasks at a synthetic 10k-in/1k-out shape.
    let price = |model: &str| -> Option<(f64, f64)> {
        modron_router::lookup(model).map(|info| (info.input_per_m, info.output_per_m))
    };
    let task_cost = |model: &str| -> f64 {
        price(model)
            .map(|(i, o)| 10_000.0 / 1e6 * i + 1_000.0 / 1e6 * o)
            .unwrap_or(0.0)
    };
    let strong_cost = task_cost(&strong);
    let cheap_cost = task_cost(&cheap);
    let routed_cost = cases
        .iter()
        .map(|c| {
            if c.chosen == modron_router::catalog::Tier::Cheap {
                cheap_cost
            } else {
                strong_cost
            }
        })
        .sum::<f64>();
    let always_strong = strong_cost * cases.len() as f64;
    let scale = 1000.0 / cases.len().max(1) as f64;
    println!("\nestimated cost per 1000 tasks (10k-in/1k-out each, catalog-priced):");
    println!("  always-strong ({strong}): ${:.2}", always_strong * scale);
    println!("  routed:                  ${:.2}", routed_cost * scale);
    println!(
        "  saved:                   ${:.2} ({:.0}%)",
        (always_strong - routed_cost) * scale,
        100.0 * (always_strong - routed_cost) / always_strong.max(f64::MIN_POSITIVE)
    );
    Ok(())
}

/// `modron chat` (Phase-1c I3): one session, many turns. Each line is a
/// task; the transcript (with compaction + memory) carries context across
/// lines. Errors don't kill the session.
async fn run_chat(cli: &Cli, cfg: &config::Config) -> anyhow::Result<()> {
    use std::io::Write as _;
    let mcp = spawn_mcp_servers(cfg).await?;
    let agent = build_agent_with(
        cli,
        cfg,
        true,
        AgentSinks::for_mode(RunMode::INTERACTIVE_STREAM),
        &mcp,
    )?;
    println!("modron chat — one session, streaming answers. /exit or Ctrl-D to quit.");
    if let Some(tag) = crate::upgrade::startup_update_notice() {
        println!("⬆ modron {tag} is available — run `modron upgrade`");
    }
    // `--resume <id>` seeds the chat instead of silently starting fresh;
    // bare `--resume` picks up the most recent session.
    let mut session_id: Option<SessionId> = match agent.resume {
        Some(id) => Some(id),
        None if cli.resume.is_some() => {
            use modron_core::SessionStore as _;
            agent
                .store
                .list()
                .await?
                .into_iter()
                .next() // list() sorts by updated_at desc
                .map(|meta| meta.id)
        }
        None => None,
    };
    if let Some(id) = session_id {
        println!("resuming session {id}");
    }
    let mut total = Usage::default();
    let stdin = std::io::stdin();
    loop {
        eprint!("» ");
        std::io::stderr().flush()?;
        let mut line = String::new();
        if stdin.read_line(&mut line)? == 0 {
            break; // EOF (Ctrl-D)
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if matches!(line, "/exit" | "/quit" | "exit" | "quit") {
            break;
        }
        let result = match session_id {
            Some(id) => agent.engine.run_task_resuming(id, line).await,
            None => agent.engine.run_task(line).await,
        };
        match result {
            Ok(summary) => {
                session_id = Some(summary.session_id);
                total.input_tokens += summary.usage.input_tokens;
                total.output_tokens += summary.usage.output_tokens;
                total.cached_input_tokens += summary.usage.cached_input_tokens;
                if summary.answer.is_empty() {
                    println!();
                }
                println!(
                    "\n[+{} tok — cumulative {} tok, {} turns, {} tool calls]",
                    summary.usage.total(),
                    total.total(),
                    summary.turns,
                    summary.tool_calls
                );
            }
            Err(err) => eprintln!("error: {err:#}\n(continuing — type the next task)"),
        }
    }
    if let Some(id) = session_id {
        println!("session: {id} — resume with --resume {id}");
    }
    Ok(())
}

/// JudgeMode from config (Phase-3b M3).
fn parse_judge_mode(mode: Option<&str>) -> anyhow::Result<modron_core::JudgeMode> {
    match mode {
        None | Some("ambiguous") => Ok(modron_core::JudgeMode::Ambiguous),
        Some("always") => Ok(modron_core::JudgeMode::Always),
        Some(other) => Err(anyhow::anyhow!(
            "unknown judge mode `{other}` — use \"ambiguous\" or \"always\""
        )),
    }
}

/// Startup update check (Phase 6e): cached 24h, 2s live timeout, never
/// fails the caller.
fn modron_upgrade_notice() -> Option<String> {
    crate::upgrade::startup_update_notice()
}

/// Char-safe truncation for one-line UI summaries.
fn short(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max_chars).collect();
        format!("{cut}…")
    }
}

fn default_sessions_dir() -> PathBuf {
    match dirs::home_dir() {
        Some(home) => home.join(".modron").join("sessions"),
        None => {
            eprintln!("warning: no home directory found; using ./.modron/sessions");
            PathBuf::from(".modron").join("sessions")
        }
    }
}

/// The workspace user-memory root for a sessions dir: `<parent>/memory`
/// (default layout `~/.modron/sessions` → `~/.modron/memory`). Deriving it
/// from `--sessions-dir` keeps runs and `modron memory` inspectable in tests.
fn memory_root_for(sessions_dir: &Path) -> PathBuf {
    match sessions_dir.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join("memory"),
        _ => PathBuf::from("memory"),
    }
}

/// `modron memory list|show|forget` (M4): user memory must be inspectable
/// and forgettable — no silent state. All three operate on the same
/// `<sessions-parent>/memory` root the run path reads and writes.
fn run_memory(action: MemoryAction, sessions_dir: &Path, working_dir: &Path) -> anyhow::Result<()> {
    let memory_root = memory_root_for(sessions_dir);
    let store = MemoryStore::for_workspace(&memory_root, working_dir)
        .map_err(|err| anyhow::anyhow!("cannot open workspace memory: {err}"))?;
    match action {
        MemoryAction::List => {
            let index = store.read_index();
            if index.trim().is_empty() {
                println!(
                    "no memory for this workspace yet ({})",
                    store.root().display()
                );
            } else {
                println!("{}", index.trim_end());
            }
        }
        MemoryAction::Show { title } => match store.read_entry(&title)? {
            Some(body) => println!("{}", body.trim_end()),
            None => anyhow::bail!("no memory entry titled '{title}' — see `modron memory list`"),
        },
        MemoryAction::Forget { title } => {
            if store.forget(&title)? {
                println!("forgot: {title}");
            } else {
                anyhow::bail!("no memory entry titled '{title}' — see `modron memory list`");
            }
        }
        MemoryAction::Stats => {
            let stats = store.stats();
            let facts = store.facts_oldest_first();
            if facts.is_empty() && stats.is_empty() {
                println!(
                    "no memory for this workspace yet ({})",
                    store.root().display()
                );
                return Ok(());
            }
            println!("usefulness ledger ({} facts):", facts.len());
            for (id, label, _) in facts {
                let stat = stats.get(&id);
                println!(
                    "  {:<9} uses={:<3} ok={:<3} fail={:<3} {}",
                    stat.map(|s| if s.relegated { "relegated" } else { "hot" })
                        .unwrap_or("new"),
                    stat.map(|s| s.uses).unwrap_or(0),
                    stat.map(|s| s.successes).unwrap_or(0),
                    stat.map(|s| s.failures).unwrap_or(0),
                    entry_title_cli(&label),
                );
            }
            let crumbs = store.breadcrumbs();
            println!(
                "breadcrumbs: {} (never injected; TTL-pruned by curate)",
                crumbs.len()
            );
        }
        MemoryAction::Curate => {
            let report = store
                .curate(modron_core::memory::CuratePolicy::default())
                .map_err(|e| anyhow::anyhow!("curate failed: {e}"))?;
            println!(
                "curated: {} facts ({} relegated, {} reinstated), {} breadcrumbs pruned",
                report.facts_total,
                report.facts_relegated,
                report.facts_reinstated,
                report.breadcrumbs_pruned
            );
            // The global tier gets the same treatment.
            if let Ok(global) = MemoryStore::for_global(&memory_root) {
                if global.root().exists() {
                    let g = global
                        .curate(modron_core::memory::CuratePolicy::default())
                        .map_err(|e| anyhow::anyhow!("global curate failed: {e}"))?;
                    println!(
                        "global: {} facts ({} relegated, {} reinstated), {} breadcrumbs pruned",
                        g.facts_total, g.facts_relegated, g.facts_reinstated, g.breadcrumbs_pruned
                    );
                }
            }
        }
    }
    Ok(())
}

/// `{YYYY-MM-DD} {title}` → `{title}` (display helper for `memory stats`).
fn entry_title_cli(label: &str) -> &str {
    label.get(11..).unwrap_or(label)
}

/// `modron rewind` (C2): restore the last checkpointed file and print the
/// truncation point. The truncation itself takes effect on the NEXT
/// `--resume` of that session (the engine skips abandoned events); the
/// append-only log is never rewritten.
async fn run_rewind(
    session: Option<SessionId>,
    sessions_dir: &Path,
    working_dir: &Path,
) -> anyhow::Result<()> {
    let store = JsonlSessionStore::new(sessions_dir);
    let session_id = match session {
        Some(id) => id,
        None => store
            .list()
            .await?
            .first()
            .map(|meta| meta.id)
            .ok_or_else(|| anyhow::anyhow!("no sessions found in {}", sessions_dir.display()))?,
    };
    let outcome = modron_core::rewind::rewind_last_checkpoint(&store, session_id, working_dir)
        .await
        .map_err(|err| anyhow::anyhow!("{err}"))?;
    if outcome.deleted {
        println!(
            "rewound: deleted {} (the agent created it)",
            outcome.restored_path
        );
    } else {
        println!("rewound: restored {}", outcome.restored_path);
    }
    println!(
        "session {} resumes from event {} ({} later events stay in the log, unused)",
        session_id, outcome.truncated_at_event, outcome.abandoned_events
    );
    // Persist the truncation point: the next `--resume <id>` picks it up
    // (a later rewind overwrites it; delete the marker to resume fully).
    let marker = sessions_dir.join(format!("{}.rewind", session_id));
    std::fs::write(&marker, outcome.truncated_at_event.to_string())
        .map_err(|err| anyhow::anyhow!("cannot write rewind marker {}: {err}", marker.display()))?;
    Ok(())
}

/// `modron sessions` (DX1): newest first.
async fn run_sessions_list(sessions_dir: &Path) -> anyhow::Result<()> {
    const SHOW: usize = 20;
    let store = JsonlSessionStore::new(sessions_dir);
    let sessions = store
        .list()
        .await
        .map_err(|err| anyhow::anyhow!("cannot list sessions: {err}"))?;
    if sessions.is_empty() {
        println!("no sessions in {}", sessions_dir.display());
        return Ok(());
    }
    println!(
        "{:<38} {:<28} {:>6} {:>10} {:>10}  title",
        "id", "updated", "msgs", "tokens", "est. cost"
    );
    for meta in sessions.iter().take(SHOW) {
        // Per-session token/cost accounting from the persisted Usage events
        // (provider-reported ground truth; cost estimated from the catalog).
        let mut usage = Usage::default();
        let mut cost = 0.0;
        if let Ok(events) = store.load(meta.id).await {
            let mut per_model: HashMap<String, Usage> = HashMap::new();
            for event in &events {
                if let SessionEvent::Usage { model, usage: u } = event {
                    usage.input_tokens += u.input_tokens;
                    usage.output_tokens += u.output_tokens;
                    usage.cached_input_tokens += u.cached_input_tokens;
                    let entry = per_model.entry(model.clone()).or_default();
                    entry.input_tokens += u.input_tokens;
                    entry.output_tokens += u.output_tokens;
                }
            }
            for (model, u) in per_model {
                if let Some(info) = modron_router::lookup(&model) {
                    cost += info.cost_usd(u.input_tokens, u.output_tokens);
                }
            }
        }
        println!(
            "{:<38} {:<28} {:>6} {:>10} {:>10}  {}",
            meta.id,
            meta.updated_at.format("%Y-%m-%d %H:%M:%S"),
            meta.message_count,
            usage.total(),
            format!("${cost:.4}"),
            meta.title
        );
    }
    if sessions.len() > SHOW {
        println!("… {} older sessions elided", sessions.len() - SHOW);
    }
    Ok(())
}

/// `modron usage` (Phase-2a D1): aggregate every session's provider-reported
/// usage events into one per-model table with cache-hit rates and
/// catalog-estimated costs. Honest zeros when nothing was cached; unknown
/// models get no invented price.
async fn run_usage(sessions_dir: &Path, last: Option<usize>) -> anyhow::Result<()> {
    let store = JsonlSessionStore::new(sessions_dir);
    let sessions = store
        .list()
        .await
        .map_err(|err| anyhow::anyhow!("cannot list sessions: {err}"))?;
    if sessions.is_empty() {
        println!("no sessions in {}", sessions_dir.display());
        return Ok(());
    }
    let shown = last.unwrap_or(sessions.len()).min(sessions.len());

    let mut order: Vec<String> = Vec::new();
    let mut per_model: HashMap<String, Usage> = HashMap::new();
    let mut sessions_with_usage = 0usize;
    let mut compactions = 0usize;
    for meta in sessions.iter().take(shown) {
        let events = store.load(meta.id).await.unwrap_or_default();
        let mut saw_usage = false;
        for event in &events {
            match event {
                SessionEvent::Usage { model, usage } => {
                    saw_usage = true;
                    if !order.contains(model) {
                        order.push(model.clone());
                    }
                    let entry = per_model.entry(model.clone()).or_default();
                    entry.input_tokens += usage.input_tokens;
                    entry.output_tokens += usage.output_tokens;
                    entry.cached_input_tokens += usage.cached_input_tokens;
                }
                SessionEvent::Compaction { .. } => compactions += 1,
                _ => {}
            }
        }
        if saw_usage {
            sessions_with_usage += 1;
        }
    }

    println!(
        "usage across {} sessions ({} with token data){}",
        shown,
        sessions_with_usage,
        if sessions.len() > shown {
            format!(" — newest {} of {}", shown, sessions.len())
        } else {
            String::new()
        }
    );
    if per_model.is_empty() {
        println!("no usage events recorded yet");
        return Ok(());
    }

    println!(
        "{:<24} {:>10} {:>10} {:>10} {:>10} {:>14}",
        "model", "input", "output", "cached", "cache hit", "est. cost"
    );
    let mut total = Usage::default();
    let mut total_cost = 0.0;
    for model in &order {
        let usage = &per_model[model];
        total.input_tokens += usage.input_tokens;
        total.output_tokens += usage.output_tokens;
        total.cached_input_tokens += usage.cached_input_tokens;
        let cost = match modron_router::lookup(model) {
            Some(info) => info.cost_usd(usage.input_tokens, usage.output_tokens),
            None => f64::NAN,
        };
        total_cost += cost;
        print_usage_row(model, usage, cost);
    }
    print_usage_row("total", &total, total_cost);
    if compactions > 0 {
        println!("autocompactions: {compactions}");
    }
    Ok(())
}

/// Compaction progress on stderr (X3), one line per compaction that happened
/// in THIS run: `compacted: N events → summary (est. saved ~X tok)`. The
/// saved estimate is the estimated size of the transcript the compaction
/// replaced (re-rendered from the events before the Compaction event with
/// the same tail rule the engine used) minus the summary's estimate — an
/// estimate, labeled as one, never a provider-reported number.
fn print_compaction_lines(events: &[SessionEvent], baseline: usize, tail_messages: usize) {
    for (idx, event) in events.iter().enumerate().skip(baseline) {
        if let SessionEvent::Compaction {
            summary,
            summarized_events,
        } = event
        {
            let older = modron_core::compact::older_transcript_text(&events[..idx], tail_messages);
            let saved = estimate_tokens(&older) as i64 - estimate_tokens(summary) as i64;
            eprintln!(
                "compacted: {summarized_events} events → summary (est. saved ~{} tok)",
                saved.max(0)
            );
        }
    }
}

/// Print the per-model usage table for THIS run: persisted `Usage` events
/// (provider-reported ground truth) appended after `baseline`, with estimated
/// cost from the router catalog and the provider-reported cache-hit rate
/// (K3): cached/(input+cached), `0%` when nothing was cached. Unknown models
/// get `n/a` instead of a made-up price.
fn print_usage_table(events: &[SessionEvent]) -> anyhow::Result<()> {
    let mut order: Vec<String> = Vec::new();
    let mut per_model: HashMap<String, Usage> = HashMap::new();
    for event in events {
        if let SessionEvent::Usage { model, usage } = event {
            if !order.contains(model) {
                order.push(model.clone());
            }
            let entry = per_model.entry(model.clone()).or_default();
            entry.input_tokens += usage.input_tokens;
            entry.output_tokens += usage.output_tokens;
            entry.cached_input_tokens += usage.cached_input_tokens;
        }
    }
    if per_model.is_empty() {
        return Ok(());
    }

    println!();
    println!(
        "usage per model (provider-reported; cost is ESTIMATED from the catalog; \
         cache hit = cached/(input+cached)):"
    );
    println!(
        "{:<24} {:>10} {:>10} {:>10} {:>10} {:>14}",
        "model", "input", "output", "cached", "cache hit", "est. cost"
    );
    let mut total = Usage::default();
    let mut total_cost = 0.0;
    for model in &order {
        let usage = &per_model[model];
        total.input_tokens += usage.input_tokens;
        total.output_tokens += usage.output_tokens;
        total.cached_input_tokens += usage.cached_input_tokens;
        let cost = match modron_router::lookup(model) {
            Some(info) => info.cost_usd(usage.input_tokens, usage.output_tokens),
            None => f64::NAN, // unknown model: no honest price to quote
        };
        total_cost += cost;
        print_usage_row(model, usage, cost);
    }
    print_usage_row("total", &total, total_cost);
    Ok(())
}

/// Provider-reported cache-hit rate as a rounded percentage: cached input
/// over all input (cached tokens ride outside `input_tokens` on some
/// providers). `0%` when nothing was cached — never a blank or a guess.
fn cache_hit_column(usage: &Usage) -> String {
    let all_input = usage.input_tokens + usage.cached_input_tokens;
    if all_input == 0 {
        return "0%".to_string();
    }
    format!(
        "{:.0}%",
        (usage.cached_input_tokens as f64 / all_input as f64) * 100.0
    )
}

fn print_usage_row(model: &str, usage: &Usage, cost: f64) {
    let cost_cell = if cost.is_nan() {
        "n/a".to_string()
    } else {
        format!("${:.4}", cost)
    };
    println!(
        "{:<24} {:>10} {:>10} {:>10} {:>10} {:>14}",
        model,
        usage.input_tokens,
        usage.output_tokens,
        usage.cached_input_tokens,
        cache_hit_column(usage),
        cost_cell
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_choice_ids_match_factory_names() {
        assert_eq!(ProviderChoice::Anthropic.id(), "anthropic");
        assert_eq!(ProviderChoice::OpenaiCompat.id(), "openai-compat");
        assert_eq!(ProviderChoice::Mock.id(), "mock");
        assert_eq!(ProviderChoice::Zai.id(), "zai");
    }

    #[test]
    fn memory_root_sits_beside_sessions_dir() {
        assert_eq!(
            memory_root_for(Path::new("/home/u/.modron/sessions")),
            PathBuf::from("/home/u/.modron/memory")
        );
        assert_eq!(
            memory_root_for(Path::new("/tmp/e2e/sessions")),
            PathBuf::from("/tmp/e2e/memory")
        );
        // Degenerate: a bare relative dir keeps memory next to it in cwd.
        assert_eq!(
            memory_root_for(Path::new("sessions")),
            PathBuf::from("memory")
        );
    }

    #[test]
    fn cache_hit_column_is_provider_reported_share() {
        let usage = Usage {
            input_tokens: 750,
            output_tokens: 10,
            cached_input_tokens: 250,
        };
        assert_eq!(cache_hit_column(&usage), "25%");
        // Nothing cached → honest 0%, not blank.
        assert_eq!(cache_hit_column(&Usage::default()), "0%");
        let no_cache = Usage {
            input_tokens: 100,
            output_tokens: 5,
            cached_input_tokens: 0,
        };
        assert_eq!(cache_hit_column(&no_cache), "0%");
    }

    #[test]
    fn compaction_lines_only_for_this_run_and_estimate_is_honest() {
        use modron_core::{Message, ToolOutcome};
        let summary = "TASK: t\nDECISIONS: none\nEDITS: none\nOPEN ITEMS: none";
        let events = vec![
            SessionEvent::Task { text: "t".into() },
            SessionEvent::Message {
                parent_id: None,
                message: Message::user("hello"),
            },
            SessionEvent::Compaction {
                summary: summary.into(),
                summarized_events: 2,
            },
            SessionEvent::ToolResult {
                call_id: "c1".into(),
                outcome: ToolOutcome::ok("recent tail result"),
            },
        ];
        // Baseline 0: the compaction is this run's → one line. The estimate
        // is computed from the re-rendered older transcript (>0 for real
        // content), never invented from thin air.
        print_compaction_lines(&events, 0, 6);
        // Baseline past the event (a resumed run): nothing printed.
        print_compaction_lines(&events, events.len(), 6);
        // A summary larger than the summarized text clamps at 0, never negative.
        let tiny = vec![
            SessionEvent::Task { text: "t".into() },
            SessionEvent::Compaction {
                summary: "x".repeat(4096),
                summarized_events: 1,
            },
        ];
        print_compaction_lines(&tiny, 0, 6);
    }
}
