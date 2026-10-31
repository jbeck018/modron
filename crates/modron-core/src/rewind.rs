//! Rewind (Phase-1b C2): restore workspace files from the last checkpoint
//! and compute where transcript reconstruction should restart. The session
//! log stays append-only — "abandoned" events remain on disk for audit but
//! never re-enter context (via `EngineConfig::resume_from_event`).

use crate::{b64, SessionEvent, SessionId, SessionStore};
use std::path::Path;

/// What a rewind changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewindOutcome {
    /// File the checkpoint protected (as recorded in the checkpoint).
    pub restored_path: String,
    /// Events from this index onward are abandoned (never re-enter context).
    /// This is the index of the `ToolResult` the checkpoint preceded.
    pub truncated_at_event: u64,
    /// How many events after the truncation point stay in the log unused.
    pub abandoned_events: usize,
    /// Whether the file was deleted (it did not exist before the checkpointed
    /// tool ran) rather than rewritten.
    pub deleted: bool,
}

/// Find the last `Checkpoint` event in the session, restore the file it
/// protects (delete it if the checkpoint says the file was new), and return
/// the truncation point for the next resume.
pub async fn rewind_last_checkpoint(
    store: &dyn SessionStore,
    session_id: SessionId,
    working_dir: &Path,
) -> crate::Result<RewindOutcome> {
    let events = store.load(session_id).await?;
    let Some((_idx, checkpoint)) = events.iter().enumerate().rev().find_map(|(i, e)| match e {
        SessionEvent::Checkpoint {
            path,
            prior_content_b64,
            event_index,
        } => Some((i, (path.clone(), prior_content_b64.clone(), *event_index))),
        _ => None,
    }) else {
        return Err(crate::Error::Session(format!(
            "session {session_id} has no checkpoints — nothing to rewind; \
             checkpoints are captured before write/edit tools run"
        )));
    };
    let (path, prior_b64, event_index) = checkpoint;

    let target = resolve_under(working_dir, &path)?;
    let deleted = match prior_b64.as_deref() {
        Some(b64_body) => {
            let bytes = b64::decode(b64_body).map_err(|e| {
                crate::Error::Session(format!("checkpoint content undecodable: {e}"))
            })?;
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    crate::Error::Session(format!("cannot recreate parent dir: {e}"))
                })?;
            }
            std::fs::write(&target, bytes)
                .map_err(|e| crate::Error::Session(format!("cannot restore {path}: {e}")))?;
            false
        }
        None => {
            // The checkpointed tool created this file — rewind deletes it.
            // Missing-already is fine (user may have removed it since).
            match std::fs::remove_file(&target) {
                Ok(()) => true,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                Err(e) => {
                    return Err(crate::Error::Session(format!(
                        "cannot delete created file {path}: {e}"
                    )))
                }
            }
        }
    };

    let abandoned = events.len().saturating_sub(event_index as usize + 1);
    Ok(RewindOutcome {
        restored_path: path,
        truncated_at_event: event_index,
        abandoned_events: abandoned,
        deleted,
    })
}

/// Resolve a checkpoint-relative path under the working dir; absolute paths
/// pass through (they were recorded as given to the tool).
fn resolve_under(working_dir: &Path, path: &str) -> crate::Result<std::path::PathBuf> {
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        Ok(p.to_path_buf())
    } else {
        Ok(working_dir.join(p))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::InMemoryStore;
    use crate::ToolOutcome;

    fn checkpoint(path: &str, prior: Option<&[u8]>, event_index: u64) -> SessionEvent {
        SessionEvent::Checkpoint {
            path: path.to_string(),
            prior_content_b64: prior.map(b64::encode),
            event_index,
        }
    }

    #[tokio::test]
    async fn restores_prior_content_and_reports_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cfg.txt");
        std::fs::write(&file, b"original").unwrap();

        let store = InMemoryStore::default();
        let meta = store.create_session("t").await.unwrap();
        store
            .append(meta.id, &SessionEvent::Task { text: "t".into() })
            .await
            .unwrap();
        // event 1 = the ToolResult that mutated the file
        store
            .append(
                meta.id,
                &SessionEvent::ToolResult {
                    call_id: "c".into(),
                    outcome: ToolOutcome::ok("x"),
                },
            )
            .await
            .unwrap();
        store
            .append(
                meta.id,
                &SessionEvent::Message {
                    parent_id: None,
                    message: crate::Message::user("more history after the fact"),
                },
            )
            .await
            .unwrap();

        // Rewind to before event 1: file must be restored to "original".
        // (Insert the checkpoint retroactively for the test by appending and
        // rewinding to it — the engine writes checkpoints before mutation.)
        store
            .append(meta.id, &checkpoint("cfg.txt", Some(b"original"), 1))
            .await
            .unwrap();
        std::fs::write(&file, b"mutated by the agent").unwrap();

        let outcome = rewind_last_checkpoint(&store, meta.id, dir.path())
            .await
            .unwrap();
        assert_eq!(outcome.restored_path, "cfg.txt");
        assert_eq!(outcome.truncated_at_event, 1);
        assert!(!outcome.deleted);
        assert_eq!(std::fs::read(&file).unwrap(), b"original");
    }

    #[tokio::test]
    async fn deletes_files_that_were_new_and_counts_abandoned() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("created.txt");
        std::fs::write(&file, b"agent-made").unwrap();

        let store = InMemoryStore::default();
        let meta = store.create_session("t").await.unwrap();
        store
            .append(meta.id, &SessionEvent::Task { text: "t".into() })
            .await
            .unwrap();
        // checkpoint at index 1 (before the tool result), tool result at 1,
        // then 3 more events after — all abandoned on rewind.
        store
            .append(meta.id, &checkpoint("created.txt", None, 1))
            .await
            .unwrap();
        for _ in 0..4 {
            store
                .append(
                    meta.id,
                    &SessionEvent::ToolResult {
                        call_id: "c".into(),
                        outcome: ToolOutcome::ok("x"),
                    },
                )
                .await
                .unwrap();
        }

        let outcome = rewind_last_checkpoint(&store, meta.id, dir.path())
            .await
            .unwrap();
        assert!(outcome.deleted);
        assert_eq!(outcome.truncated_at_event, 1);
        assert_eq!(outcome.abandoned_events, 4);
        assert!(!file.exists());
    }

    #[tokio::test]
    async fn no_checkpoints_is_a_clear_error() {
        let store = InMemoryStore::default();
        let meta = store.create_session("t").await.unwrap();
        let err = rewind_last_checkpoint(&store, meta.id, Path::new("."))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no checkpoints"));
    }
}
