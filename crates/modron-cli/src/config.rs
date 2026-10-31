//! L2 — Modron configuration: optional TOML file + environment overrides.
//!
//! Precedence: env > file > none. Config problems are never fatal: a missing
//! file is normal, a malformed file warns and falls back to defaults, and a
//! malformed numeric env var warns and is ignored (the file value survives).
//! Nothing in this module panics.
//!
//! File location: `~/.config/modron/config.toml`. Env keys:
//! `MODRON_DEFAULT_PROVIDER`, `MODRON_DEFAULT_MODEL`,
//! `MODRON_MAX_INPUT_TOKENS`, `MODRON_MAX_TOTAL_TOKENS`. An empty env value
//! counts as unset (so wrappers can pass through unset vars harmlessly).
//!
//! API keys (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `OPENAI_BASE_URL`, …) are
//! read by the provider factory (modron-providers, P3), not here.

use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Runtime configuration for the CLI. Every field is optional; when a field
/// is absent the engine defaults apply (`modron_core::EngineConfig`).
#[derive(Deserialize, Default, Debug, Clone, PartialEq)]
pub struct Config {
    /// Provider id, e.g. "anthropic", "openai-compat", "mock".
    pub default_provider: Option<String>,
    /// Model name, e.g. "claude-sonnet-4-5".
    pub default_model: Option<String>,
    /// Per-turn assembled-context cap (pre-flight token estimate).
    pub max_input_tokens: Option<u64>,
    /// Cumulative token budget for a whole task run.
    pub max_total_tokens: Option<u64>,
    /// Lifecycle hooks around tool calls (Phase-1c I4): `pre_tool` may veto
    /// (non-zero exit blocks the tool), `post_tool` observes. Shell commands
    /// receiving the `MODRON_TOOL_*` env contract.
    #[serde(default)]
    pub hooks: Option<HooksConfig>,
    /// Autocompaction tuning (Phase-2a D2); CLI flags override these.
    #[serde(default)]
    pub compaction: Option<CompactionConfig>,
    /// The project's verify command (Phase-2a D3): when set, the `verify`
    /// tool is registered and the agent is told to run it after edits.
    #[serde(default)]
    pub verify: Option<VerifyConfig>,
    /// Router tuning (Phase-3a R1): tier models, mode, per-action overrides.
    #[serde(default)]
    pub router: Option<RouterConfig>,
    /// LLM routing judge (Phase-3b M3): any OpenAI-compatible provider as a
    /// pure routing agent (e.g. Ollama + a 0.5B model locally).
    #[serde(default)]
    pub judge: Option<JudgeConfig>,
    /// Kev / TypeSafe Jev SystemOne endpoint (Phase-3b M2): a local or
    /// hosted decision model (wire-compatible APIs).
    #[serde(default)]
    pub kev: Option<KevConfig>,
    /// MCP servers (Phase 5a): stdio servers whose tools join the registry
    /// as `mcp__<server>__<tool>`, with per-server schema token accounting.
    #[serde(default)]
    pub mcp: Option<McpConfig>,
    /// Named agent personas (competitive review A1): tailored system prompt,
    /// forced model, tool allow/deny. Selected with `--agent <name>`.
    #[serde(default)]
    pub agents: Option<std::collections::BTreeMap<String, AgentPersona>>,
}

/// `[agents.<name>]` — a Kilo-style custom mode.
#[derive(Deserialize, Default, Debug, Clone, PartialEq, Eq)]
pub struct AgentPersona {
    /// Replaces the assembled system prompt entirely when set.
    pub system: Option<String>,
    /// Force every routing decision onto this model.
    pub model: Option<String>,
    /// Extra tool allow list (merged with --allow).
    pub allow: Option<Vec<String>>,
    /// Extra tool deny list (merged with --deny; wins).
    pub deny: Option<Vec<String>>,
}

/// `[mcp]` section (Phase 5a).
#[derive(Deserialize, Default, Debug, Clone, PartialEq, Eq)]
pub struct McpConfig {
    /// Server name → launch config. Names appear in tool ids.
    #[serde(default)]
    pub servers: std::collections::BTreeMap<String, McpServerConfig>,
}

/// One MCP stdio server.
#[derive(Deserialize, Default, Debug, Clone, PartialEq, Eq)]
pub struct McpServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Option<Vec<String>>,
    #[serde(default)]
    pub env: Option<std::collections::BTreeMap<String, String>>,
    /// Refuse the server if its tools' combined schema cost exceeds this
    /// (prompt budget is a product principle; default 20_000 tokens).
    pub max_schema_tokens: Option<u64>,
}

/// `[judge]` section (Phase-3b M3).
#[derive(Deserialize, Default, Debug, Clone, PartialEq, Eq)]
pub struct JudgeConfig {
    /// "ambiguous" (default: only unconfident sync decisions) or "always".
    pub mode: Option<String>,
    /// Judge model (default: the cheap-tier model).
    pub model: Option<String>,
    /// Provider id (default: the run's default provider).
    pub provider: Option<String>,
}

/// `[kev]` section (Phase-3b M3).
#[derive(Deserialize, Default, Debug, Clone, PartialEq, Eq)]
pub struct KevConfig {
    /// Base URL of the Kev/Jev server, e.g. http://127.0.0.1:8009.
    pub endpoint: String,
    /// Decision model id (default "kev-latest").
    pub model: Option<String>,
    /// Optional bearer token (KEV_API_KEY-equivalent).
    pub api_key: Option<String>,
    /// Same gating as `[judge]`.
    pub mode: Option<String>,
}

/// `[router]` section (Phase-3a R1).
#[derive(Deserialize, Default, Debug, Clone, PartialEq)]
pub struct RouterConfig {
    /// Cheap-tier model (default: claude-haiku-4-5).
    pub cheap_model: Option<String>,
    /// Strong-tier model (default: claude-sonnet-4-5).
    pub strong_model: Option<String>,
    /// "learned" (default; corpus centroids with heuristic fallback) or
    /// "heuristic" (rules only).
    pub mode: Option<String>,
    /// Per-action overrides: `plan = "glm-5.3"` etc. Win over tier rules.
    #[serde(default)]
    pub actions: Option<std::collections::BTreeMap<String, String>>,
}

/// `[compaction]` section (Phase-2a D2). Every field optional — unset fields
/// keep the engine default (`modron_core::CompactionPolicy`).
#[derive(Deserialize, Default, Debug, Clone, PartialEq)]
pub struct CompactionConfig {
    pub enabled: Option<bool>,
    pub threshold_frac: Option<f64>,
    pub min_interval_turns: Option<u32>,
    pub tail_messages: Option<usize>,
    pub summary_max_bytes: Option<usize>,
}

/// `[verify]` section (Phase-2a D3).
#[derive(Deserialize, Default, Debug, Clone, PartialEq, Eq)]
pub struct VerifyConfig {
    /// The shell command to run, e.g. `cargo test --quiet`.
    pub command: String,
}

/// `[hooks]` section of config.toml (Phase-1c I4).
#[derive(Deserialize, Default, Debug, Clone, PartialEq, Eq)]
pub struct HooksConfig {
    /// Run before every tool call; non-zero exit or timeout vetoes the call.
    pub pre_tool: Option<String>,
    /// Run after every tool call; failures never block.
    pub post_tool: Option<String>,
}

/// `~/.config/modron/config.toml`, or `None` when no home directory is
/// known (the rest of the stack must treat that as "no config file").
pub fn config_path() -> Option<PathBuf> {
    // `MODRON_CONFIG` overrides the default location — how tests and
    // multi-project setups inject configuration.
    if let Ok(path) = std::env::var("MODRON_CONFIG") {
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    dirs::home_dir().map(|home| home.join(".config").join("modron").join("config.toml"))
}

/// Load config from the default path with process-environment overrides.
/// Never fails: see the module docs for the degradation ladder.
pub fn load() -> Config {
    let env_reader = |key: &str| std::env::var(key).ok();
    match config_path() {
        Some(path) => load_from(&path, env_reader),
        None => {
            tracing::warn!("no home directory found; skipping config file, using environment only");
            apply_env(Config::default(), &env_reader)
        }
    }
}

/// Pure, testable loader: the file at `path` is the base layer (missing file
/// = empty base; malformed file = warn + default base), then `env` overrides
/// win. The `env` closure makes this side-effect free and testable.
pub fn load_from(path: &Path, env: impl Fn(&str) -> Option<String>) -> Config {
    let base = read_config_file(path)
        .and_then(|src| parse_config(&src))
        .unwrap_or_default();
    apply_env(base, &env)
}

/// Read the file. `None` = "no usable file contents" (missing, unreadable,
/// or — after [`parse_config`] — malformed); a missing file is normal and
/// stays quiet, anything else warns.
fn read_config_file(path: &Path) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(src) => Some(src),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "cannot read config file; ignoring it"
            );
            None
        }
    }
}

/// Parse the TOML. Malformed files degrade to `None` (→ defaults) with a
/// warning; they never fail the run.
fn parse_config(src: &str) -> Option<Config> {
    match toml::from_str(src) {
        Ok(config) => Some(config),
        Err(error) => {
            tracing::warn!(%error, "malformed config file; ignoring it and using defaults");
            None
        }
    }
}

/// Apply env overrides on top of a base config. String keys override when
/// non-empty; numeric keys must parse as `u64` or they warn and are ignored.
fn apply_env(mut config: Config, env: &impl Fn(&str) -> Option<String>) -> Config {
    if let Some(value) = non_empty(env("MODRON_DEFAULT_PROVIDER")) {
        config.default_provider = Some(value);
    }
    if let Some(value) = non_empty(env("MODRON_DEFAULT_MODEL")) {
        config.default_model = Some(value);
    }
    if let Some(raw) = non_empty(env("MODRON_MAX_INPUT_TOKENS")) {
        if let Some(n) = parse_env_number("MODRON_MAX_INPUT_TOKENS", &raw) {
            config.max_input_tokens = Some(n);
        }
    }
    if let Some(raw) = non_empty(env("MODRON_MAX_TOTAL_TOKENS")) {
        if let Some(n) = parse_env_number("MODRON_MAX_TOTAL_TOKENS", &raw) {
            config.max_total_tokens = Some(n);
        }
    }
    config
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}

fn parse_env_number(key: &str, raw: &str) -> Option<u64> {
    match raw.trim().parse::<u64>() {
        Ok(n) => Some(n),
        Err(error) => {
            tracing::warn!(key, value = %raw, %error, "ignoring non-numeric env override");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn write_config(dir: &Path, contents: &str) -> PathBuf {
        let path = dir.join("config.toml");
        fs::write(&path, contents).expect("write temp config");
        path
    }

    #[test]
    fn file_only_values_are_used() {
        let dir = tempdir().expect("tempdir");
        let path = write_config(
            dir.path(),
            "default_provider = 'openai-compat'\n\
             default_model = 'gpt-5-mini'\n\
             max_input_tokens = 100_000\n\
             max_total_tokens = 1_000_000\n",
        );
        let config = load_from(&path, no_env);
        assert_eq!(config.default_provider.as_deref(), Some("openai-compat"));
        assert_eq!(config.default_model.as_deref(), Some("gpt-5-mini"));
        assert_eq!(config.max_input_tokens, Some(100_000));
        assert_eq!(config.max_total_tokens, Some(1_000_000));
    }

    #[test]
    fn env_overrides_file() {
        let dir = tempdir().expect("tempdir");
        let path = write_config(
            dir.path(),
            "default_provider = 'openai-compat'\n\
             default_model = 'gpt-5-mini'\n\
             max_input_tokens = 100_000\n",
        );
        let config = load_from(&path, |key| match key {
            "MODRON_DEFAULT_MODEL" => Some("claude-haiku-4-5".to_string()),
            "MODRON_MAX_INPUT_TOKENS" => Some("999".to_string()),
            _ => None,
        });
        // File value kept where env is silent.
        assert_eq!(config.default_provider.as_deref(), Some("openai-compat"));
        // Env wins where env speaks.
        assert_eq!(config.default_model.as_deref(), Some("claude-haiku-4-5"));
        assert_eq!(config.max_input_tokens, Some(999));
    }

    #[test]
    fn missing_everything_yields_all_none() {
        let config = load_from(Path::new("/nonexistent/modron/config.toml"), no_env);
        assert_eq!(config, Config::default());
        assert!(config.default_provider.is_none());
        assert!(config.default_model.is_none());
        assert!(config.max_input_tokens.is_none());
        assert!(config.max_total_tokens.is_none());
    }

    #[test]
    fn malformed_toml_falls_back_to_default() {
        let dir = tempdir().expect("tempdir");
        let path = write_config(dir.path(), "default_provider = [oops, not toml");
        let config = load_from(&path, no_env);
        assert_eq!(config, Config::default());
    }

    #[test]
    fn unknown_file_fields_are_tolerated() {
        // Forward compatibility: newer config files must not break older CLIs.
        let dir = tempdir().expect("tempdir");
        let path = write_config(
            dir.path(),
            "default_model = 'claude-haiku-4-5'\nfuture_option = 'whatever'\n",
        );
        let config = load_from(&path, no_env);
        assert_eq!(config.default_model.as_deref(), Some("claude-haiku-4-5"));
    }

    #[test]
    fn bad_numeric_env_is_ignored_and_file_value_survives() {
        let dir = tempdir().expect("tempdir");
        let path = write_config(
            dir.path(),
            "max_input_tokens = 5_000\nmax_total_tokens = 10_000\n",
        );
        let config = load_from(&path, |key| match key {
            "MODRON_MAX_INPUT_TOKENS" => Some("abc".to_string()),
            "MODRON_MAX_TOTAL_TOKENS" => Some("12.5".to_string()),
            _ => None,
        });
        assert_eq!(config.max_input_tokens, Some(5_000));
        assert_eq!(config.max_total_tokens, Some(10_000));
    }

    #[test]
    fn bad_numeric_env_with_no_file_stays_none() {
        let config = load_from(
            Path::new("/nonexistent/modron/config.toml"),
            |key| match key {
                "MODRON_MAX_TOTAL_TOKENS" => Some("not-a-number".to_string()),
                _ => None,
            },
        );
        assert_eq!(config.max_total_tokens, None);
    }

    #[test]
    fn empty_env_values_count_as_unset() {
        let config = load_from(
            Path::new("/nonexistent/modron/config.toml"),
            |key| match key {
                "MODRON_DEFAULT_PROVIDER" => Some("   ".to_string()),
                _ => None,
            },
        );
        assert_eq!(config.default_provider, None);
    }

    #[test]
    fn config_path_points_at_modron_config() {
        if let Some(path) = config_path() {
            assert!(
                path.ends_with(".config/modron/config.toml"),
                "path: {path:?}"
            );
        }
        // None (no home dir) is equally acceptable; nothing to assert.
    }

    #[test]
    fn hooks_section_parses_and_defaults_to_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[hooks]\npre_tool = \"echo check\"\npost_tool = \"echo done\"\n",
        )
        .unwrap();
        let config = load_from(&path, no_env);
        let hooks = config.hooks.expect("hooks section parsed");
        assert_eq!(hooks.pre_tool.as_deref(), Some("echo check"));
        assert_eq!(hooks.post_tool.as_deref(), Some("echo done"));

        let empty = dir.path().join("empty.toml");
        std::fs::write(&empty, "default_model = \"m\"\n").unwrap();
        assert!(load_from(&empty, no_env).hooks.is_none());
    }

    #[test]
    fn compaction_and_verify_sections_parse() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[compaction]\nthreshold_frac = 0.7\ntail_messages = 8\n\n[verify]\ncommand = \"cargo test --quiet\"\n",
        )
        .unwrap();
        let config = load_from(&path, no_env);
        let compaction = config.compaction.expect("compaction section parsed");
        assert_eq!(compaction.threshold_frac, Some(0.7));
        assert_eq!(compaction.tail_messages, Some(8));
        assert_eq!(compaction.enabled, None, "unset fields stay None");
        assert_eq!(
            config.verify.expect("verify parsed").command,
            "cargo test --quiet"
        );
    }
}
