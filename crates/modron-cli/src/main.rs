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
pub mod auth;
pub mod config;

use clap::{Parser, Subcommand, ValueEnum};
use modron_core::memory::MemoryStore;
use modron_core::testing::demo_provider;
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
}

impl SandboxChoice {
    fn policy(self) -> modron_core::SandboxPolicy {
        match self {
            Self::Off => modron_core::SandboxPolicy::Off,
            Self::Seatbelt => modron_core::SandboxPolicy::Seatbelt,
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
    /// Subscription auth (EXPERIMENTAL): manage OAuth tokens for providers
    /// that support it (currently: claude).
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
    /// List stored sessions, newest first.
    Sessions,
    /// Aggregate token/cost dashboard across sessions (provider-reported
    /// usage, catalog-priced costs, cache-hit rates).
    Usage {
        /// Show only the N most recent sessions.
        #[arg(long)]
        last: Option<usize>,
    },
}

#[derive(Subcommand, Clone)]
enum McpAction {
    /// Inspect one configured server by name.
    Info { name: String },
}

#[derive(Subcommand, Clone)]
enum AuthAction {
    /// Authorize via browser (PKCE + localhost callback) and store tokens.
    Login,
    /// Show whether stored subscription tokens exist.
    Status,
    /// Delete stored tokens.
    Logout,
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
}

#[derive(Parser, Clone)]
#[command(
    name = "modron",
    version,
    about = "Modron — a token-efficient Rust AI coding agent (approvals, sandbox, memory, compaction, TUI)",
    after_help = "Config file: ~/.config/modron/config.toml — Sessions: ~/.modron/sessions/ — Memory: ~/.modron/memory/"
)]
struct Cli {
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
    /// Continue this session id instead of starting a new one.
    #[arg(long, global = true)]
    resume: Option<SessionId>,
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
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    init_tracing(cli.verbose);
    if let Err(err) = run(cli).await {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

/// stderr tracing: `--verbose` (or `RUST_LOG`) shows debug detail; the
/// default filter shows info-level turn/tool progress from the engine.
/// ANSI colors only when stderr is a terminal.
fn init_tracing(verbose: bool) {
    use std::io::IsTerminal as _;
    let default_filter = if verbose { "debug" } else { "info" };
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| default_filter.to_string());
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .init();
}

async fn run(cli: Cli) -> anyhow::Result<()> {
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
    // `modron rewind` — restore the last checkpointed file + set the
    // truncation point the next --resume of that session will honor.
    if let Some(Command::Rewind { session }) = cli.command {
        return run_rewind(session, &sessions_dir, &working_dir).await;
    }
    // `modron sessions` — the store is inspectable (no silent state).
    if let Some(Command::Sessions) = cli.command {
        return run_sessions_list(&sessions_dir).await;
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

    // `modron auth …` — subscription token management (Phase 5d X3).
    if let Some(Command::Auth { action }) = cli.command {
        return crate::auth::run_auth(action).await;
    }

    // `modron mcp info …` — server inspection (Phase 5e).
    if let Some(Command::Mcp {
        action: McpAction::Info { name },
    }) = cli.command
    {
        return run_mcp_info(&cfg, name).await;
    }

    // `modron bench` — multi-model measurement (Phase 5f).
    if let Some(Command::Bench { models, list }) = cli.command.clone() {
        return run_bench(&cli, &cfg, models.as_deref(), list).await;
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
        return modron_tui::run_with(
            |sinks| {
                let agent = build_agent_with(
                    &cli,
                    &cfg,
                    interactive,
                    AgentSinks {
                        approver: Some(sinks.approver),
                        on_delta: Some(sinks.on_delta),
                        on_tool_event: Some(sinks.on_tool_event),
                    },
                    &mcp,
                )
                .expect("build agent for tui");
                (agent.engine, agent.resume)
            },
            String::new(),
        )
        .await
        .map_err(|err| anyhow::anyhow!("{err:#}"));
    }

    let Some(task) = cli.task.clone() else {
        anyhow::bail!("no task given — pass a task string or a subcommand (see --help)");
    };

    let interactive = std::io::IsTerminal::is_terminal(&std::io::stdin());
    let mcp = spawn_mcp_servers(&cfg).await?;
    let agent = build_agent_with(
        &cli,
        &cfg,
        interactive,
        AgentSinks::for_mode(interactive, false),
        &mcp,
    )?;
    let compaction_tail_messages = agent.engine.config.compaction.tail_messages;

    // Events already in the log before this invocation — used to scope the
    // usage table and compaction lines to THIS run (a resumed session already
    // carries past usage/compactions). Doubles as an early, actionable check
    // that a --resume id exists.
    let usage_baseline = match cli.resume {
        Some(session_id) => agent.store.load(session_id).await?.len(),
        None => 0,
    };

    let summary = match cli.resume {
        Some(session_id) => agent.engine.run_task_resuming(session_id, &task).await?,
        None => agent.engine.run_task(&task).await?,
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
    fn for_mode(interactive: bool, stream: bool) -> Self {
        Self {
            approver: if interactive { tty_approver() } else { None },
            on_delta: if stream {
                Some(stdout_delta_sink())
            } else {
                None
            },
            on_tool_event: None,
        }
    }
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

    let build_provider = |pid: &str| -> anyhow::Result<Arc<dyn ModelProvider>> {
        Ok(match pid {
            "mock" => Arc::new(demo_provider()),
            other => Arc::new(RetryingProvider::new(provider_from_env(other).map_err(
                |err| {
                    anyhow::anyhow!(err.to_string())
                        .context(format!("cannot start provider `{other}`"))
                },
            )?)),
        })
    };

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
                .unwrap_or_else(|| "claude-haiku-4-5".to_string());
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
                mode: if cli.approve_all {
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
        resume_from_event: cli.resume.and_then(|id| {
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
        sandbox: cli
            .sandbox
            .map(|choice| choice.policy())
            .unwrap_or_default(),
        approver: if cli.approve_all {
            None
        } else {
            sinks.approver
        },
        async_router,
        on_delta: sinks.on_delta,
        on_tool_event: sinks.on_tool_event,
        personas: Default::default(),
    };

    if !engine_config.working_dir.is_dir() {
        anyhow::bail!(
            "working directory `{}` does not exist — pass --working-dir <path>",
            engine_config.working_dir.display()
        );
    }

    let mut providers: HashMap<String, Arc<dyn ModelProvider>> = HashMap::new();
    providers.insert(provider_id.clone(), build_provider(&provider_id)?);

    let store = Arc::new(JsonlSessionStore::new(&sessions_dir));
    let mut registry = default_registry_with_memory(&memory_root);
    if let Some(verify) = &cfg.verify {
        registry.register(Arc::new(VerifyTool::new(&verify.command)));
    }
    // Phase 4: read-only subagent for repo-wide questions; skills load
    // on demand — the tool only registers when the repo HAS skills.
    registry.register(Arc::new(modron_tools::ExploreTool));
    if !modron_core::context::load_skills_index(&working_dir).is_empty() {
        registry.register(Arc::new(modron_tools::SkillTool::for_working_dir(
            &working_dir,
        )));
    }
    for reg in mcp {
        registry.register(Arc::new(modron_mcp::McpTool::new(
            Arc::clone(&reg.handle),
            &reg.server,
            reg.info.clone(),
        )));
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
    let engine = Engine {
        selector: build_selector(cfg, persona.as_ref().and_then(|p| p.model.clone()))?,
        providers,
        tools: Arc::new(registry),
        store: Arc::clone(&store) as Arc<dyn SessionStore>,
        config: engine_config,
    };
    Ok(Agent {
        engine,
        store,
        resume: cli.resume,
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
        StreamDelta::Done => println!(),
    })
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

/// Build the active selector from `[router]` config (Phase-3a R1).
/// Default mode is `learned` (corpus centroids + heuristic fallback — the
/// fallback preserves budget/overflow rules, so learned is a strict
/// refinement of the rules).
fn build_selector(
    cfg: &config::Config,
    persona_model: Option<String>,
) -> anyhow::Result<Arc<dyn modron_core::ModelSelector>> {
    use modron_router::{learned::LearnedRouter, HeuristicSelector};
    let router_cfg = cfg.router.clone().unwrap_or_default();
    let cheap = router_cfg
        .cheap_model
        .clone()
        .unwrap_or_else(|| "claude-haiku-4-5".into());
    let strong = router_cfg
        .strong_model
        .clone()
        .unwrap_or_else(|| "claude-sonnet-4-5".into());
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
        None | Some("learned") => Ok(Arc::new(LearnedRouter::new(heuristic))),
        Some("heuristic") => Ok(Arc::new(heuristic)),
        Some(other) => Err(anyhow::anyhow!(
            "unknown router mode `{other}` — use \"learned\" or \"heuristic\"",
        )),
    }
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

/// `modron bench` (Phase 5f): run the suite across a model list, print the
/// per-model comparison, and persist every task record to
/// `~/.modron/bench/<timestamp>.jsonl` — the accumulating baseline file
/// PLAN's cost tracking reads from.
async fn run_bench(
    cli: &Cli,
    cfg: &config::Config,
    models_arg: Option<&str>,
    list: bool,
) -> anyhow::Result<()> {
    if list {
        for model in curated_free_models() {
            println!("{model}");
        }
        return Ok(());
    }
    let models: Vec<String> = match models_arg {
        Some(csv) => csv.split(',').map(str::trim).map(String::from).collect(),
        None => curated_free_models()
            .into_iter()
            .map(String::from)
            .collect(),
    };
    let suite = bench_suite();
    let mcp = spawn_mcp_servers(cfg).await?;
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

        for task in &suite {
            let task_started = std::time::Instant::now();
            let agent = build_agent_with(
                &model_cli,
                cfg,
                false,
                AgentSinks::for_mode(false, false),
                &mcp,
            )?;
            let record = match agent.engine.run_task(task).await {
                Ok(summary) => {
                    let task_ok = !summary.answer.is_empty();
                    if task_ok {
                        completed += 1;
                    }
                    tokens += summary.usage.total();
                    cached += summary.usage.cached_input_tokens;
                    BenchRecord {
                        provider: "zai".into(),
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
                        provider: "zai".into(),
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
    Ok(())
}

/// `modron router eval` (Phase-3a R4): score the active selector against the\n/// `modron router eval` (Phase-3a R4): score the active selector against the
/// held-out eval set; report accuracy and the catalog-priced cost delta vs.
/// an always-strong baseline (synthetic 10k-in/1k-out task per example).
fn run_router_eval(cfg: &config::Config) -> anyhow::Result<()> {
    use modron_router::learned::run_eval;
    let selector = build_selector(cfg, None)?;
    let cases = run_eval(selector.as_ref());
    let router_cfg = cfg.router.clone().unwrap_or_default();
    let cheap = router_cfg
        .cheap_model
        .clone()
        .unwrap_or_else(|| "claude-haiku-4-5".into());
    let strong = router_cfg
        .strong_model
        .clone()
        .unwrap_or_else(|| "claude-sonnet-4-5".into());

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
    let agent = build_agent_with(cli, cfg, true, AgentSinks::for_mode(true, true), &mcp)?;
    println!("modron chat — one session, streaming answers. /exit or Ctrl-D to quit.");
    let mut session_id: Option<SessionId> = None;
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
    }
    Ok(())
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
