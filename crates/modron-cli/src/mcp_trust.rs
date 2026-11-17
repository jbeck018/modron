//! Project MCP servers and their trust (Program V, V1).
//!
//! `<workspace>/.mcp.json` and `<workspace>/.cursor/mcp.json` declare MCP
//! servers in the shape other agents use:
//! `{"mcpServers": {name: {command, args, env} | {url, headers}}}`.
//! A declared server is never started unless the user trusted it for this
//! workspace. Trust is stored in `mcp-trust.json` next to `config.toml` and
//! keyed by the canonical workspace path, the server name and a hash of the
//! server definition, so a changed definition needs trust again. Servers in
//! Modron's own `[mcp.servers]` win on a name clash and never need trust.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::{self, McpServerConfig};

/// Files searched for project MCP servers, in precedence order (on a name
/// clash between the two, the first file wins).
pub const DISCOVERY_FILES: [&str; 2] = [".mcp.json", ".cursor/mcp.json"];

/// Trust store file name, placed in the Modron config directory.
const TRUST_FILE: &str = "mcp-trust.json";

/// One server as a project file declares it.
#[derive(Debug, Clone)]
struct Declared {
    name: String,
    source: &'static str,
    /// `sha256:<hex>` over the server's raw JSON definition.
    hash: String,
    /// The parsed server, or why it is invalid.
    parsed: Result<McpServerConfig, String>,
}

/// Where a declared server stands for this workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Trusted for this workspace and this exact definition: it may start.
    Trusted,
    /// Declared but not trusted (or the definition changed since trust).
    Untrusted,
    /// The declaration does not parse or fails validation.
    Invalid(String),
    /// Modron's own `[mcp.servers]` already defines this name.
    ShadowedByConfig,
    /// An earlier project file already defines this name.
    ShadowedBy(&'static str),
}

/// One row of `modron mcp list`.
#[derive(Debug, Clone)]
pub struct Status {
    pub name: String,
    pub source: &'static str,
    pub hash: String,
    pub state: State,
    /// Present for every valid declaration (trusted or not).
    pub config: Option<McpServerConfig>,
}

/// `sha256:<hex>` of a server definition. serde_json keeps object keys sorted
/// (no `preserve_order` feature), so the serialized form is canonical.
pub fn definition_hash(raw: &serde_json::Value) -> String {
    let canonical = serde_json::to_string(raw).unwrap_or_default();
    let digest = Sha256::digest(canonical.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

/// Read every project MCP file under `workspace`. Missing files are normal;
/// unreadable or malformed ones are reported in the returned error list.
fn declared_servers(workspace: &Path) -> (Vec<Declared>, Vec<String>) {
    let mut out = Vec::new();
    let mut errors = Vec::new();
    for rel in DISCOVERY_FILES {
        let text = match std::fs::read_to_string(workspace.join(rel)) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                errors.push(format!("{rel}: cannot read ({err})"));
                continue;
            }
        };
        let root: serde_json::Value = match serde_json::from_str(&text) {
            Ok(root) => root,
            Err(err) => {
                errors.push(format!("{rel}: not valid JSON ({err})"));
                continue;
            }
        };
        let Some(servers) = root.get("mcpServers") else {
            continue;
        };
        let Some(map) = servers.as_object() else {
            errors.push(format!("{rel}: `mcpServers` must be an object"));
            continue;
        };
        for (name, raw) in map {
            let parsed = serde_json::from_value::<McpServerConfig>(raw.clone())
                .map_err(|err| err.to_string())
                .and_then(|server| server.validate(name).map(|()| server));
            out.push(Declared {
                name: name.clone(),
                source: rel,
                hash: definition_hash(raw),
                parsed,
            });
        }
    }
    (out, errors)
}

/// Every project MCP server in `workspace` with its state under `store`.
/// `configured` lists the names Modron's own `[mcp.servers]` defines.
/// Returns the rows plus file-level errors.
pub fn statuses(
    workspace: &Path,
    configured: &[String],
    store: &TrustStore,
) -> (Vec<Status>, Vec<String>) {
    let key = workspace_key(workspace);
    let (declared, errors) = declared_servers(workspace);
    let mut claimed: Vec<(String, &'static str)> = Vec::new();
    let mut rows = Vec::new();
    for decl in declared {
        let shadow = claimed
            .iter()
            .find(|(name, _)| *name == decl.name)
            .map(|(_, source)| *source);
        let (state, config) = if configured.contains(&decl.name) {
            (State::ShadowedByConfig, None)
        } else if let Some(source) = shadow {
            (State::ShadowedBy(source), None)
        } else {
            claimed.push((decl.name.clone(), decl.source));
            match decl.parsed {
                Err(why) => (State::Invalid(why), None),
                Ok(server) => {
                    let state = if store.is_trusted(&key, &decl.name, &decl.hash) {
                        State::Trusted
                    } else {
                        State::Untrusted
                    };
                    (state, Some(server))
                }
            }
        };
        rows.push(Status {
            name: decl.name,
            source: decl.source,
            hash: decl.hash,
            state,
            config,
        });
    }
    (rows, errors)
}

/// Project servers that may start for `workspace`: trusted, valid, and not
/// shadowed. Reads the trust store at the default path; an unreadable store
/// starts nothing (fail closed) and logs a warning.
pub fn trusted_servers(workspace: &Path, configured: &[String]) -> Vec<(String, McpServerConfig)> {
    let store = match TrustStore::open(trust_store_path()) {
        Ok(store) => store,
        Err(err) => {
            tracing::warn!(error = %err, "MCP trust store unreadable; project MCP servers stay off");
            return Vec::new();
        }
    };
    let (rows, errors) = statuses(workspace, configured, &store);
    for error in errors {
        tracing::warn!(error = %error, "project MCP file ignored");
    }
    rows.into_iter()
        .filter_map(|row| match (row.state, row.config) {
            (State::Trusted, Some(server)) => Some((row.name, server)),
            _ => None,
        })
        .collect()
}

/// Names defined by Modron's own `[mcp.servers]`.
pub fn configured_names(cfg: &config::Config) -> Vec<String> {
    cfg.mcp
        .as_ref()
        .map(|mcp| mcp.servers.keys().cloned().collect())
        .unwrap_or_default()
}

/// Config for `modron mcp info|read <name>`: when Modron's own config does not
/// define `name` but a project file does, add that server to a copy of the
/// config, but only if it is trusted. An untrusted or invalid project server
/// is an error here, so it is never started. Unknown names pass through and
/// get the caller's usual "unknown server" error.
pub fn with_project_server(cfg: &config::Config, name: &str) -> anyhow::Result<config::Config> {
    if cfg
        .mcp
        .as_ref()
        .is_some_and(|mcp| mcp.servers.contains_key(name))
    {
        return Ok(cfg.clone());
    }
    let workspace = current_workspace()?;
    let store = TrustStore::open(trust_store_path())
        .map_err(|err| anyhow!("MCP trust store unreadable: {err}"))?;
    let (rows, _) = statuses(&workspace, &configured_names(cfg), &store);
    let Some(row) = rows.into_iter().find(|row| row.name == name) else {
        return Ok(cfg.clone());
    };
    match (row.state, row.config) {
        (State::Trusted, Some(server)) => {
            let mut out = cfg.clone();
            out.mcp
                .get_or_insert_with(Default::default)
                .servers
                .insert(name.to_string(), server);
            Ok(out)
        }
        (State::Untrusted, _) => bail!(
            "MCP server `{name}` is declared in {} and is not trusted for this workspace; \
             run `modron mcp trust {name}` to allow it",
            row.source
        ),
        (State::Invalid(why), _) => {
            bail!("MCP server `{name}` in {} is invalid: {why}", row.source)
        }
        (State::ShadowedBy(source), _) => bail!(
            "MCP server `{name}` in {} is shadowed by an earlier file ({source})",
            row.source
        ),
        (State::ShadowedByConfig, _) | (State::Trusted, None) => Ok(cfg.clone()),
    }
}

/// The Modron config directory's trust file, next to `config.toml`.
pub fn trust_store_path() -> Option<PathBuf> {
    config::config_path()?
        .parent()
        .map(|dir| dir.join(TRUST_FILE))
}

/// Canonical workspace path as the trust key (falls back to the path as given
/// when it cannot be canonicalized, which keeps the key stable enough).
pub fn workspace_key(workspace: &Path) -> String {
    std::fs::canonicalize(workspace)
        .unwrap_or_else(|_| workspace.to_path_buf())
        .to_string_lossy()
        .to_string()
}

fn current_workspace() -> anyhow::Result<PathBuf> {
    std::env::current_dir().context("cannot read the current directory")
}

/// One trust grant: this server definition may run in this workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustEntry {
    pub workspace: String,
    pub server: String,
    pub hash: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct TrustFileDoc {
    #[serde(default)]
    trusted: Vec<TrustEntry>,
}

/// The trust file. `path` is `None` when no config directory exists; then
/// nothing can be saved and nothing is trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustStore {
    path: Option<PathBuf>,
    entries: Vec<TrustEntry>,
}

impl TrustStore {
    /// Open the store at `path`. A missing file is an empty store; a malformed
    /// file is an error (so `trust` never overwrites grants it cannot read).
    pub fn open(path: Option<PathBuf>) -> Result<Self, String> {
        let Some(file) = path.as_ref() else {
            return Ok(Self {
                path,
                entries: Vec::new(),
            });
        };
        let entries = match std::fs::read_to_string(file) {
            Ok(text) => {
                serde_json::from_str::<TrustFileDoc>(&text)
                    .map_err(|err| format!("{} is not valid ({err})", file.display()))?
                    .trusted
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(err) => return Err(format!("cannot read {} ({err})", file.display())),
        };
        Ok(Self { path, entries })
    }

    pub fn is_trusted(&self, workspace: &str, server: &str, hash: &str) -> bool {
        self.entries
            .iter()
            .any(|e| e.workspace == workspace && e.server == server && e.hash == hash)
    }

    /// Grant trust for this definition, replacing any earlier grant for the
    /// same workspace and server (so an old hash cannot linger).
    pub fn trust(&mut self, workspace: &str, server: &str, hash: &str) {
        self.entries
            .retain(|e| !(e.workspace == workspace && e.server == server));
        self.entries.push(TrustEntry {
            workspace: workspace.to_string(),
            server: server.to_string(),
            hash: hash.to_string(),
        });
    }

    /// Remove grants for `workspace`: one server, or all when `server` is None.
    /// Returns how many were removed.
    pub fn untrust(&mut self, workspace: &str, server: Option<&str>) -> usize {
        let before = self.entries.len();
        self.entries
            .retain(|e| !(e.workspace == workspace && server.is_none_or(|name| e.server == name)));
        before - self.entries.len()
    }

    /// Write the store atomically (temp file, then rename).
    pub fn save(&self) -> anyhow::Result<()> {
        let Some(file) = self.path.as_ref() else {
            bail!("no home directory found; the MCP trust file cannot be stored");
        };
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("cannot create {}", dir.display()))?;
        }
        let doc = TrustFileDoc {
            trusted: self.entries.clone(),
        };
        let text = serde_json::to_string_pretty(&doc)?;
        let tmp = file.with_extension("json.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("cannot write {}", tmp.display()))?;
        std::fs::rename(&tmp, file)
            .with_context(|| format!("cannot replace {}", file.display()))?;
        Ok(())
    }
}

fn open_store() -> anyhow::Result<TrustStore> {
    TrustStore::open(trust_store_path()).map_err(|err| anyhow!("{err}"))
}

/// `modron mcp trust [name | --all]`: trust one declared server, or every
/// valid undeclared-by-config server, for the current workspace.
pub fn run_trust(cfg: &config::Config, name: Option<&str>, all: bool) -> anyhow::Result<()> {
    if name.is_some() == all {
        bail!("name one server (`modron mcp trust <name>`) or trust all of them (`--all`)");
    }
    let workspace = current_workspace()?;
    let key = workspace_key(&workspace);
    let mut store = open_store()?;
    let (rows, errors) = statuses(&workspace, &configured_names(cfg), &store);
    for error in &errors {
        eprintln!("warning: {error}");
    }
    let targets: Vec<Status> = match name {
        Some(name) => {
            let row = rows
                .into_iter()
                .find(|row| row.name == name)
                .ok_or_else(|| {
                    anyhow!("no project MCP server named `{name}` (see `modron mcp list`)")
                })?;
            match &row.state {
                State::Invalid(why) => bail!("MCP server `{name}` is invalid: {why}"),
                State::ShadowedByConfig => {
                    bail!("MCP server `{name}` is defined in [mcp.servers]; it needs no trust")
                }
                State::ShadowedBy(source) => {
                    bail!("MCP server `{name}` is shadowed by an earlier file ({source})")
                }
                _ => vec![row],
            }
        }
        None => rows
            .into_iter()
            .filter(|row| row.config.is_some())
            .filter(|row| row.state != State::ShadowedByConfig)
            .collect(),
    };
    if targets.is_empty() {
        println!("no project MCP servers to trust in {key}");
        return Ok(());
    }
    for row in &targets {
        store.trust(&key, &row.name, &row.hash);
        println!("trusted `{}` ({}) for {key}", row.name, row.source);
    }
    store.save()?;
    println!("a changed definition needs trust again; `modron mcp untrust` revokes");
    Ok(())
}

/// `modron mcp untrust [name]`: revoke trust for the current workspace.
pub fn run_untrust(name: Option<&str>) -> anyhow::Result<()> {
    let key = workspace_key(&current_workspace()?);
    let mut store = open_store()?;
    let removed = store.untrust(&key, name);
    store.save()?;
    println!("removed {removed} trust grant(s) for {key}");
    Ok(())
}

/// `modron mcp list`: every project MCP server with its trust state and the
/// file that declares it.
pub fn run_list(cfg: &config::Config) -> anyhow::Result<()> {
    let workspace = current_workspace()?;
    let store = open_store()?;
    let (rows, errors) = statuses(&workspace, &configured_names(cfg), &store);
    for error in &errors {
        println!("warning: {error}");
    }
    if rows.is_empty() {
        println!(
            "no project MCP servers in {} (looked at {})",
            workspace.display(),
            DISCOVERY_FILES.join(", ")
        );
        return Ok(());
    }
    for row in &rows {
        println!(
            "{:<20} {:<28} {}",
            row.name,
            state_label(&row.state),
            row.source
        );
    }
    Ok(())
}

/// The state words shared by `list` and `doctor`.
fn state_label(state: &State) -> String {
    match state {
        State::Trusted => "trusted".to_string(),
        State::Untrusted => "untrusted (not started)".to_string(),
        State::Invalid(why) => format!("invalid: {why}"),
        State::ShadowedByConfig => "shadowed by [mcp.servers]".to_string(),
        State::ShadowedBy(source) => format!("shadowed by {source}"),
    }
}

/// Lines for `modron doctor`: rule files, project commands and project MCP
/// servers found in the current workspace, with trust state.
pub fn doctor_lines(cfg: &config::Config) -> Vec<String> {
    let mut lines = Vec::new();
    let Ok(workspace) = current_workspace() else {
        return vec!["  · no current directory; project files not checked".to_string()];
    };
    lines.push(format!("  workspace: {}", workspace.display()));
    let rules = modron_core::context::project_rule_sources(&workspace);
    if rules.is_empty() {
        lines.push("  · rules: none (.cursor/rules, .github/copilot-instructions.md, GEMINI.md, .windsurfrules)".to_string());
    } else {
        lines.push(format!("  ✓ rules: {}", rules.join(", ")));
    }
    let commands = modron_core::context::project_command_names(&workspace);
    if commands.is_empty() {
        lines.push("  · commands: none (.claude/commands/*.md)".to_string());
    } else {
        let slashes: Vec<String> = commands.iter().map(|c| format!("/{c}")).collect();
        lines.push(format!("  ✓ commands: {}", slashes.join(", ")));
    }
    let store = match open_store() {
        Ok(store) => store,
        Err(err) => {
            lines.push(format!("  ✗ mcp trust: {err}"));
            TrustStore {
                path: None,
                entries: Vec::new(),
            }
        }
    };
    let (rows, errors) = statuses(&workspace, &configured_names(cfg), &store);
    for error in errors {
        lines.push(format!("  ✗ mcp file: {error}"));
    }
    if rows.is_empty() {
        lines.push("  · mcp servers: none declared in .mcp.json or .cursor/mcp.json".to_string());
    }
    for row in rows {
        let mark = match row.state {
            State::Trusted => "✓",
            State::Untrusted => "·",
            _ => "✗",
        };
        let hint = if row.state == State::Untrusted {
            format!(" — `modron mcp trust {}`", row.name)
        } else {
            String::new()
        };
        lines.push(format!(
            "  {mark} mcp {}: {} ({}){hint}",
            row.name,
            state_label(&row.state),
            row.source
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write(dir: &Path, rel: &str, body: &serde_json::Value) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body.to_string()).unwrap();
    }

    fn store_in(dir: &Path) -> TrustStore {
        TrustStore::open(Some(dir.join("mcp-trust.json"))).unwrap()
    }

    #[test]
    fn hash_follows_the_definition_not_key_order() {
        let a = json!({"command": "srv", "args": ["x"]});
        let b = json!({"args": ["x"], "command": "srv"});
        assert_eq!(definition_hash(&a), definition_hash(&b));
        let changed = json!({"command": "srv", "args": ["y"]});
        assert_ne!(definition_hash(&a), definition_hash(&changed));
        assert!(definition_hash(&a).starts_with("sha256:"));
    }

    #[test]
    fn discovery_reads_both_files_and_flags_invalid_entries() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            ".mcp.json",
            &json!({"mcpServers": {
                "local": {"command": "srv", "args": ["a"]},
                "remote": {"url": "https://example.test/mcp", "headers": {"A": "b"}},
                "broken": {"args": ["no command or url"]}
            }}),
        );
        write(
            dir.path(),
            ".cursor/mcp.json",
            &json!({"mcpServers": {"cursor-only": {"command": "c"}, "local": {"command": "dup"}}}),
        );
        let (rows, errors) = declared_servers(dir.path());
        assert!(errors.is_empty(), "{errors:?}");
        let names: Vec<(&str, &str)> = rows.iter().map(|d| (d.name.as_str(), d.source)).collect();
        assert!(names.contains(&("local", ".mcp.json")));
        assert!(names.contains(&("cursor-only", ".cursor/mcp.json")));
        let broken = rows.iter().find(|d| d.name == "broken").unwrap();
        assert!(broken.parsed.is_err());

        let store = store_in(dir.path());
        let configured: Vec<String> = Vec::new();
        let (statuses, _) = statuses(dir.path(), &configured, &store);
        let dup = statuses
            .iter()
            .find(|s| s.name == "local" && s.source == ".cursor/mcp.json")
            .unwrap();
        assert_eq!(dup.state, State::ShadowedBy(".mcp.json"));
        let broken = statuses.iter().find(|s| s.name == "broken").unwrap();
        assert!(matches!(broken.state, State::Invalid(_)));
    }

    #[test]
    fn malformed_project_file_is_reported_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".mcp.json"), "{ not json").unwrap();
        let (rows, errors) = declared_servers(dir.path());
        assert!(rows.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("not valid JSON"), "{errors:?}");
    }

    #[test]
    fn config_name_wins_and_is_never_trust_gated() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            ".mcp.json",
            &json!({"mcpServers": {"docs": {"command": "x"}}}),
        );
        let store = store_in(dir.path());
        let (rows, _) = statuses(dir.path(), &["docs".to_string()], &store);
        assert_eq!(rows[0].state, State::ShadowedByConfig);
    }

    #[test]
    fn untrusted_by_default_then_trusted_then_changed_definition_untrusts() {
        let dir = tempfile::tempdir().unwrap();
        let key = workspace_key(dir.path());
        write(
            dir.path(),
            ".mcp.json",
            &json!({"mcpServers": {"demo": {"command": "srv", "args": ["1"]}}}),
        );
        let mut store = store_in(dir.path());
        let (rows, _) = statuses(dir.path(), &[], &store);
        assert_eq!(rows[0].state, State::Untrusted);

        store.trust(&key, "demo", &rows[0].hash);
        store.save().unwrap();
        let reopened = store_in(dir.path());
        let (rows, _) = statuses(dir.path(), &[], &reopened);
        assert_eq!(rows[0].state, State::Trusted);

        write(
            dir.path(),
            ".mcp.json",
            &json!({"mcpServers": {"demo": {"command": "srv", "args": ["2"]}}}),
        );
        let (rows, _) = statuses(dir.path(), &[], &reopened);
        assert_eq!(
            rows[0].state,
            State::Untrusted,
            "changed definition must need trust again"
        );
    }

    #[test]
    fn trust_store_round_trips_replaces_and_untrusts() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = store_in(dir.path());
        store.trust("/ws", "a", "sha256:1");
        store.trust("/ws", "a", "sha256:2");
        store.trust("/ws", "b", "sha256:3");
        store.trust("/other", "a", "sha256:9");
        store.save().unwrap();

        let mut reopened = store_in(dir.path());
        assert!(
            !reopened.is_trusted("/ws", "a", "sha256:1"),
            "old hash must not linger"
        );
        assert!(reopened.is_trusted("/ws", "a", "sha256:2"));
        assert_eq!(reopened.untrust("/ws", Some("a")), 1);
        assert_eq!(reopened.untrust("/ws", None), 1);
        assert!(
            reopened.is_trusted("/other", "a", "sha256:9"),
            "other workspaces untouched"
        );
    }

    #[test]
    fn malformed_trust_file_is_an_error_not_an_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("mcp-trust.json"), "garbage").unwrap();
        assert!(TrustStore::open(Some(dir.path().join("mcp-trust.json"))).is_err());
    }

    #[test]
    fn no_config_directory_means_nothing_is_trusted_and_nothing_saves() {
        let store = TrustStore::open(None).unwrap();
        assert!(!store.is_trusted("/ws", "a", "sha256:1"));
        assert!(store.save().is_err());
    }
}
