//! automations.rs — scheduled prompts on `modron serve` (the paradev
//! automations grammar, interval-based): a named prompt runs every N
//! minutes into a FRESH chat session, results land in the normal session
//! store (visible on every client).
//!
//! Registry persists at `<working_dir>/.modron/automations.json`; the
//! scheduler wakes every 30 seconds and fires anything due.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use modron_protocol::AutomationWire;
use serde::{Deserialize, Serialize};

use crate::dispatch::ServerState;

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

fn registry_path(state: &ServerState) -> PathBuf {
    state.working_dir.join(".modron").join("automations.json")
}

pub fn load(state: &ServerState) -> Vec<Automation> {
    let path = registry_path(state);
    match std::fs::read(&path) {
        Ok(data) => serde_json::from_slice(&data).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

pub fn save(state: &ServerState, automations: &[Automation]) -> Result<()> {
    let path = registry_path(state);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Write-then-rename: a torn truncate-in-place write would be silently
    // swallowed by load's unwrap_or_default and PERMANENTLY wipe the
    // registry (cycle-2 audit L3; the session store has had this since
    // phase 1).
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(automations)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
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

/// The scheduler loop: wakes every 30 seconds, fires due automations.
/// Each fire creates a FRESH session (the automation prompt is the task)
/// so results accumulate in the normal store and every client can open
/// them.
pub fn spawn_scheduler(state: Arc<ServerState>) {
    tokio::spawn(async move {
        let state = state;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            for automation in load(&state) {
                let due = match automation.next_due_epoch {
                    Some(due) => now >= due,
                    None => true,
                };
                if !due {
                    continue;
                }
                if let Err(e) = fire(&state, &automation).await {
                    tracing::warn!(
                        automation = %automation.name,
                        error = %e,
                        "automation run failed"
                    );
                }
            }
        }
    });
}

/// One fire: fresh session, the automation prompt as the task, note the
/// run in the registry.
async fn fire(state: &Arc<ServerState>, automation: &Automation) -> Result<()> {
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
        entry.next_due_epoch = Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() + (automation.interval_minutes as u64) * 60)
                .unwrap_or(u64::MAX),
        );
        let _ = save(state, &all);
    }
    Ok(())
}

/// Re-compute next-due stamps on registry load so a fresh serve doesn't
/// re-fire everything immediately.
pub fn normalize(state: &ServerState) {
    normalize_registry(&registry_path(state));
}

fn normalize_registry(path: &std::path::Path) {
    let mut all: Vec<Automation> = match std::fs::read(path) {
        Ok(data) => serde_json::from_slice(&data).unwrap_or_default(),
        Err(_) => return,
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut changed = false;
    for automation in &mut all {
        if automation.next_due_epoch.is_none() {
            automation.next_due_epoch = Some(now + (automation.interval_minutes as u64) * 60);
            changed = true;
        }
    }
    if changed {
        let _ = std::fs::write(path, serde_json::to_string_pretty(&all).unwrap_or_default());
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
}
