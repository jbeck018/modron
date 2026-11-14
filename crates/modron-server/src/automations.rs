//! automations.rs - scheduled prompts on `modron serve` (the paradev
//! automations grammar, interval-based): a named prompt runs every N
//! minutes into a FRESH chat session, results land in the normal session
//! store (visible on every client).
//!
//! The registry persists in the USER data dir, never in the repository:
//! `<modron home>/automations/<first 16 hex of sha256(canonical working dir)>.json`
//! (mode 0600, written via temp file + rename). A cloned repository that
//! ships `.modron/automations.json` is NOT read, migrated or written; the
//! server logs a warning naming that file and ignores it (B5).
//!
//! Intervals below [`MIN_INTERVAL_MINUTES`] are rejected on add and clamped
//! up on load. The scheduler wakes every 30 seconds, fires anything due, and
//! skips an automation whose previous run is still active.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use modron_core::SessionId;
use modron_protocol::AutomationWire;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::dispatch::ServerState;

/// Smallest interval an automation may fire at (B5).
pub const MIN_INTERVAL_MINUTES: u32 = 5;

/// One automation: a prompt on an interval.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Automation {
    pub name: String,
    pub prompt: String,
    pub interval_minutes: u32,
    /// RFC3339 timestamp of the last fired run (empty = never).
    #[serde(default)]
    pub last_run: String,
    /// Internal: next due epoch seconds. Persisted when set; a registry
    /// that lacks the stamp (older version, hand-edited) is normalized to
    /// `now + interval` at serve start so nothing mass-fires on restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_due_epoch: Option<u64>,
}

/// The registry file for this server's working dir. The modron home is the
/// parent of the device-token directory (`~/.modron` by default), so the
/// registry lives under the user data dir and tests can point it at a temp
/// dir through the existing `clients_dir` option.
pub fn registry_path(state: &ServerState) -> Result<PathBuf> {
    let home = match state.devices_dir.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => crate::pairing::default_modron_dir()?,
    };
    Ok(home
        .join("automations")
        .join(format!("{}.json", working_dir_key(&state.working_dir))))
}

/// First 16 hex chars of sha256 over the canonical working dir path.
pub fn working_dir_key(working_dir: &Path) -> String {
    let canonical =
        std::fs::canonicalize(working_dir).unwrap_or_else(|_| working_dir.to_path_buf());
    let digest = Sha256::digest(canonical.to_string_lossy().as_bytes());
    digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Raise every interval below the floor to the floor. Returns true when
/// anything changed.
fn clamp_intervals(all: &mut [Automation]) -> bool {
    let mut changed = false;
    for automation in all.iter_mut() {
        if automation.interval_minutes < MIN_INTERVAL_MINUTES {
            automation.interval_minutes = MIN_INTERVAL_MINUTES;
            changed = true;
        }
    }
    changed
}

/// Directory builder for the registry's parent: owner-only (0700).
#[cfg(unix)]
fn private_dir_builder() -> std::fs::DirBuilder {
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder
}

#[cfg(not(unix))]
fn private_dir_builder() -> std::fs::DirBuilder {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    builder
}

/// Open options for the registry temp file: owner-only (0600) from creation.
#[cfg(unix)]
fn private_file_options() -> std::fs::OpenOptions {
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = std::fs::OpenOptions::new();
    options.mode(0o600);
    options
}

#[cfg(not(unix))]
fn private_file_options() -> std::fs::OpenOptions {
    std::fs::OpenOptions::new()
}

/// Write-then-rename: a torn truncate-in-place write would be silently
/// swallowed by the loader's unwrap_or_default and PERMANENTLY wipe the
/// registry (cycle-2 audit L3). The temp file is created 0600 so the
/// registry is never readable by other local users.
fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        private_dir_builder().create(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let _ = std::fs::remove_file(&tmp);
    {
        let mut file = private_file_options()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn read_registry(path: &Path) -> Vec<Automation> {
    match std::fs::read(path) {
        Ok(data) => {
            let mut all: Vec<Automation> = serde_json::from_slice(&data).unwrap_or_default();
            clamp_intervals(&mut all);
            all
        }
        Err(_) => Vec::new(),
    }
}

pub fn load(state: &ServerState) -> Vec<Automation> {
    match registry_path(state) {
        Ok(path) => read_registry(&path),
        Err(_) => Vec::new(),
    }
}

pub fn save(state: &ServerState, automations: &[Automation]) -> Result<()> {
    let path = registry_path(state)?;
    let mut owned = automations.to_vec();
    // Never persist a sub-floor interval, whatever the caller passed in.
    clamp_intervals(&mut owned);
    write_private_atomic(&path, serde_json::to_string_pretty(&owned)?.as_bytes())
}

pub async fn list(state: &ServerState) -> Vec<AutomationWire> {
    load(state)
        .into_iter()
        .map(|a| AutomationWire {
            name: a.name,
            prompt: a.prompt,
            interval_minutes: a.interval_minutes,
            last_run: (!a.last_run.is_empty()).then(|| a.last_run.clone()),
        })
        .collect()
}

pub async fn add(state: &ServerState, automation: Automation) -> Result<()> {
    if automation.interval_minutes < MIN_INTERVAL_MINUTES {
        return Err(anyhow!(
            "interval must be at least {MIN_INTERVAL_MINUTES} minutes (got {})",
            automation.interval_minutes
        ));
    }
    let mut all = load(state);
    if all.iter().any(|a| a.name == automation.name) {
        return Err(anyhow!("automation '{}' already exists", automation.name));
    }
    all.push(automation);
    save(state, &all)
}

pub async fn remove(state: &ServerState, name: &str) -> Result<bool> {
    let mut all = load(state);
    let before = all.len();
    all.retain(|a| a.name != name);
    let removed = all.len() != before;
    if removed {
        save(state, &all)?;
    }
    Ok(removed)
}

fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn session_busy(state: &ServerState, id: &SessionId) -> bool {
    state.hub.handle(id).is_some_and(|handle| handle.is_busy())
}

/// Pick the automations due at `now`. An automation whose run started by an
/// earlier fire is still active is skipped: its next-due stamp is left
/// untouched, so it fires on the first tick after that run ends. `running`
/// maps automation name -> the session of its last fire; entries for
/// removed automations are pruned.
fn select_due(
    automations: &[Automation],
    now: u64,
    running: &mut HashMap<String, SessionId>,
    is_busy: impl Fn(&SessionId) -> bool,
) -> Vec<Automation> {
    running.retain(|name, _| automations.iter().any(|a| &a.name == name));
    let mut due = Vec::new();
    for automation in automations {
        let is_due = match automation.next_due_epoch {
            Some(stamp) => now >= stamp,
            None => true,
        };
        if !is_due {
            continue;
        }
        if let Some(session) = running.get(&automation.name).copied() {
            if is_busy(&session) {
                tracing::debug!(
                    automation = %automation.name,
                    "automation skipped: previous run still active"
                );
                continue;
            }
            running.remove(&automation.name);
        }
        due.push(automation.clone());
    }
    due
}

/// The scheduler loop: wakes every 30 seconds, fires due automations.
/// Each fire creates a FRESH session (the automation prompt is the task)
/// so results accumulate in the normal store and every client can open
/// them.
pub fn spawn_scheduler(state: Arc<ServerState>) {
    tokio::spawn(async move {
        let state = state;
        let mut running: HashMap<String, SessionId> = HashMap::new();
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            let due = select_due(&load(&state), now_epoch(), &mut running, |session| {
                session_busy(&state, session)
            });
            for automation in due {
                match fire(&state, &automation).await {
                    Ok(session) => {
                        running.insert(automation.name.clone(), session);
                    }
                    Err(e) => tracing::warn!(
                        automation = %automation.name,
                        error = %e,
                        "automation run failed"
                    ),
                }
            }
        }
    });
}

/// One fire: fresh session, the automation prompt as the task, note the
/// run in the registry. Returns the session the run is writing to.
async fn fire(state: &Arc<ServerState>, automation: &Automation) -> Result<SessionId> {
    let meta = state.store.create_session(&automation.name).await?;
    // NO pre-appended Task event: run_task_with_attachments appends its own,
    // so the old pre-append duplicated the prompt in every automation
    // transcript (dispatch's adoption path never pre-appends either;
    // cycle-2 audit L3).
    let handle = state.hub.ensure_session(meta.id, Some(meta.id));
    let engine = handle.engine_cell().clone();
    // Scheduler runs carry no frame sinks; results persist in the store.
    let run: crate::hub::RunFuture = {
        let task = automation.prompt.clone();
        Box::pin(async move { engine.run_task_with_attachments(&task, Vec::new()).await })
    };
    if let Err(e) = state.hub.start_run(meta.id, run) {
        return Err(anyhow!("run failed: {e}"));
    }

    // Update the registry: last_run stamp + next due.
    let mut all = load(state);
    if let Some(entry) = all.iter_mut().find(|a| a.name == automation.name) {
        entry.last_run = chrono::Utc::now().to_rfc3339();
        entry.next_due_epoch =
            Some(now_epoch().saturating_add((automation.interval_minutes as u64) * 60));
        let _ = save(state, &all);
    }
    Ok(meta.id)
}

/// Startup hook (called by `bind_server`): warn about a repo-shipped
/// registry, then re-compute next-due stamps on the user-dir registry so a
/// fresh serve doesn't re-fire everything immediately.
pub fn normalize(state: &ServerState) {
    warn_in_repo_registry(&state.working_dir);
    if let Ok(path) = registry_path(state) {
        normalize_registry(&path);
    }
}

fn warn_in_repo_registry(working_dir: &Path) {
    let in_repo = working_dir.join(".modron").join("automations.json");
    if in_repo.is_file() {
        tracing::warn!(
            path = %in_repo.display(),
            "ignoring in-repo automations registry: automations are read only from the user data dir; this file is not loaded or migrated"
        );
    }
}

fn normalize_registry(path: &Path) {
    let mut all: Vec<Automation> = match std::fs::read(path) {
        Ok(data) => match serde_json::from_slice(&data) {
            Ok(all) => all,
            Err(_) => return,
        },
        Err(_) => return,
    };
    let now = now_epoch();
    let mut changed = clamp_intervals(&mut all);
    for automation in &mut all {
        if automation.next_due_epoch.is_none() {
            automation.next_due_epoch =
                Some(now.saturating_add((automation.interval_minutes as u64) * 60));
            changed = true;
        }
    }
    if changed {
        if let Ok(json) = serde_json::to_string_pretty(&all) {
            let _ = write_private_atomic(path, json.as_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_stamps_unstamped_entries_and_keeps_stamped_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("automations.json");
        std::fs::write(
            &path,
            r#"[
            {"name":"stale","prompt":"p","interval_minutes":10},
            {"name":"fresh","prompt":"p","interval_minutes":10,"next_due_epoch":99999999999}
        ]"#,
        )
        .unwrap();
        normalize_registry(&path);
        let all: Vec<Automation> = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let stale = all.iter().find(|a| a.name == "stale").unwrap();
        let fresh = all.iter().find(|a| a.name == "fresh").unwrap();
        assert!(
            stale.next_due_epoch.is_some(),
            "unstamped entry must gain a stamp"
        );
        assert_eq!(
            fresh.next_due_epoch,
            Some(99999999999),
            "stamped entry must be left alone"
        );
    }

    #[test]
    fn normalize_ignores_missing_registry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.json");
        normalize_registry(&path);
        assert!(!path.exists(), "no registry file must be created");
    }

    /// A server state whose modron home is `root/home` (via the device-token
    /// dir) and whose working dir is `root/work`.
    fn test_state(root: &Path) -> Arc<ServerState> {
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let store = modron_store::JsonlSessionStore::new(root.join("sessions"));
        Arc::new(ServerState {
            hub: crate::hub::SessionHub::empty(),
            store: Arc::new(store),
            devices_dir: root.join("home").join("clients"),
            version: "test".into(),
            working_dir: work,
            tasks: crate::tasks::TaskRegistry::new(),
            bridge_base: None,
            rc_keys: std::sync::Mutex::new(Default::default()),
            rc_tunnels: std::sync::Mutex::new(Default::default()),
        })
    }

    fn entry(name: &str, interval: u32, due: Option<u64>) -> Automation {
        Automation {
            name: name.into(),
            prompt: "do the thing".into(),
            interval_minutes: interval,
            last_run: String::new(),
            next_due_epoch: due,
        }
    }

    #[tokio::test]
    async fn in_repo_registry_is_ignored() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        let repo_file = state.working_dir.join(".modron").join("automations.json");
        std::fs::create_dir_all(repo_file.parent().unwrap()).unwrap();
        let shipped =
            r#"[{"name":"evil","prompt":"exfiltrate","interval_minutes":5,"next_due_epoch":1}]"#;
        std::fs::write(&repo_file, shipped).unwrap();

        assert!(
            load(&state).is_empty(),
            "repo-shipped automation must not load"
        );
        assert!(list(&state).await.is_empty());
        normalize(&state);
        assert!(load(&state).is_empty());
        assert_eq!(std::fs::read_to_string(&repo_file).unwrap(), shipped);
    }

    #[tokio::test]
    async fn add_rejects_intervals_below_floor() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        assert!(add(&state, entry("zero", 0, None)).await.is_err());
        assert!(add(&state, entry("four", 4, None)).await.is_err());
        assert!(load(&state).is_empty(), "rejected adds must not persist");
        add(&state, entry("five", 5, None)).await.unwrap();
        let rows = list(&state).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].interval_minutes, 5);
    }

    #[tokio::test]
    async fn on_disk_interval_is_clamped_on_load_and_normalize() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        let path = registry_path(&state).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"[{"name":"fast","prompt":"p","interval_minutes":1,"next_due_epoch":1}]"#,
        )
        .unwrap();

        assert_eq!(load(&state)[0].interval_minutes, MIN_INTERVAL_MINUTES);
        normalize(&state);
        let on_disk: Vec<Automation> =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(on_disk[0].interval_minutes, MIN_INTERVAL_MINUTES);
    }

    #[test]
    fn overlapping_fire_is_skipped_until_the_run_ends() {
        let due = entry("digest", 5, Some(10));
        let session = SessionId::new_v4();
        let mut running = HashMap::new();
        running.insert("digest".to_string(), session);

        // Previous run still active: skipped, nothing selected, tracking kept.
        let picked = select_due(std::slice::from_ref(&due), 100, &mut running, |_| true);
        assert!(picked.is_empty());
        assert!(running.contains_key("digest"));
        assert_eq!(due.next_due_epoch, Some(10), "skip must not move next-due");

        // Run ended: the same tick now fires and the tracking is cleared.
        let picked = select_due(std::slice::from_ref(&due), 100, &mut running, |_| false);
        assert_eq!(picked.len(), 1);
        assert!(!running.contains_key("digest"));
    }

    #[tokio::test]
    async fn registry_lands_in_user_dir_not_working_dir() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        add(&state, entry("nightly", 60, None)).await.unwrap();

        let path = registry_path(&state).unwrap();
        assert!(path.starts_with(root.path().join("home").join("automations")));
        assert!(path.is_file());
        let stem = path.file_stem().unwrap().to_string_lossy().to_string();
        assert_eq!(stem.len(), 16);
        assert!(stem.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(stem, working_dir_key(&state.working_dir));
        assert!(!state.working_dir.join(".modron").exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "registry must be owner-only");
        }
    }
}
