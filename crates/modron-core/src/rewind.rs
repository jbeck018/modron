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
            uncaptured,
        } => Some((
            i,
            (
                path.clone(),
                prior_content_b64.clone(),
                *event_index,
                *uncaptured,
            ),
        )),
        _ => None,
    }) else {
        return Err(crate::Error::Session(format!(
            "session {session_id} has no checkpoints — nothing to rewind; \
             checkpoints are captured before write/edit tools run"
        )));
    };
    let (path, prior_b64, event_index, uncaptured) = checkpoint;

    // D8: an uncaptured original is neither restorable nor known to be new.
    // Refuse before touching the disk so nothing is lost.
    if uncaptured && prior_b64.is_none() {
        return Err(crate::Error::Session(format!(
            "cannot rewind {path}: the original was not captured (too large or \
             unreadable); restore it from git"
        )));
    }

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

/// Where a fork at user turn `turn` cuts a session log (Program V4). A user
/// turn is one `Task` event, the row the transcript shows as the task echo.
/// `turn` is 1-based; `None` is the tip (every event). Returns the cut (the
/// number of leading events the fork copies) and the number of user turns
/// those events hold. Cutting at turn `n` keeps everything up to the start
/// of turn `n + 1`, so the turn's answer and its tool activity come along.
/// Pure: the caller decides where the copy goes.
pub fn fork_cut(events: &[SessionEvent], turn: Option<u32>) -> crate::Result<(usize, u32)> {
    let starts: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e, SessionEvent::Task { .. }))
        .map(|(i, _)| i)
        .collect();
    let total = starts.len() as u32;
    let Some(n) = turn else {
        return Ok((events.len(), total));
    };
    if n == 0 {
        return Err(crate::Error::Session(
            "turns are numbered from 1; /fork 0 is not a turn".into(),
        ));
    }
    if n > total {
        return Err(crate::Error::Session(if total == 0 {
            "the session has no user turns to fork at".into()
        } else {
            format!("turn {n} is out of range; the session has {total} user turns (1 to {total})")
        }));
    }
    // Turn n+1 starts at starts[n]; for the last turn the cut is the tip.
    let cut = starts.get(n as usize).copied().unwrap_or(events.len());
    Ok((cut, n))
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
            uncaptured: false,
        }
    }

    fn uncaptured_checkpoint(path: &str, event_index: u64) -> SessionEvent {
        SessionEvent::Checkpoint {
            path: path.to_string(),
            prior_content_b64: None,
            event_index,
            uncaptured: true,
        }
    }

    /// D8: an uncaptured original (too large or unreadable) must never be
    /// deleted or overwritten by rewind. Rewind refuses and touches nothing.
    #[tokio::test]
    async fn uncaptured_checkpoint_refuses_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("big.bin");
        std::fs::write(&file, b"agent-mutated bytes").unwrap();

        let store = InMemoryStore::default();
        let meta = store.create_session("t").await.unwrap();
        store
            .append(meta.id, &SessionEvent::Task { text: "t".into() })
            .await
            .unwrap();
        store
            .append(meta.id, &uncaptured_checkpoint("big.bin", 1))
            .await
            .unwrap();

        let err = rewind_last_checkpoint(&store, meta.id, dir.path())
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("not captured"),
            "unexpected error: {err}"
        );
        assert!(
            err.to_string().contains("big.bin"),
            "error must name the file: {err}"
        );
        assert_eq!(std::fs::read(&file).unwrap(), b"agent-mutated bytes");
    }

    /// Old session logs predate the `uncaptured` field: they must still
    /// parse, and a `None` checkpoint without the field keeps its original
    /// meaning (the file was new, so rewind deletes it).
    #[test]
    fn old_format_checkpoint_parses_and_means_absent() {
        let raw =
            r#"{"type":"checkpoint","path":"new.txt","prior_content_b64":null,"event_index":7}"#;
        let event: SessionEvent = serde_json::from_str(raw).expect("old log line must parse");
        match event {
            SessionEvent::Checkpoint {
                path,
                prior_content_b64,
                event_index,
                uncaptured,
            } => {
                assert_eq!(path, "new.txt");
                assert_eq!(prior_content_b64, None);
                assert_eq!(event_index, 7);
                assert!(!uncaptured, "absent default must not read as uncaptured");
            }
            other => panic!("expected checkpoint, got {other:?}"),
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

#[cfg(test)]
mod fork_cut_tests {
    use super::*;
    use crate::Message;

    fn task(text: &str) -> SessionEvent {
        SessionEvent::Task { text: text.into() }
    }

    fn reply(text: &str) -> SessionEvent {
        SessionEvent::Message {
            parent_id: None,
            message: Message::user(text),
        }
    }

    /// Three user turns; each has an answer, and turn 2 has a checkpoint.
    fn three_turns() -> Vec<SessionEvent> {
        vec![
            task("one"),
            reply("answer one"),
            task("two"),
            reply("answer two"),
            SessionEvent::Checkpoint {
                path: "a.txt".into(),
                prior_content_b64: None,
                event_index: 4,
                uncaptured: false,
            },
            task("three"),
            reply("answer three"),
        ]
    }

    #[test]
    fn team_stage_notes_do_not_move_turns_or_the_cut() {
        // A Progress note after every event (a team run's stage notes) leaves
        // the turn count and the kept conversation unchanged.
        let plain = three_turns();
        let note = |n: usize| SessionEvent::Progress {
            text: format!("note {n}"),
        };
        let mut noted = Vec::new();
        for (n, event) in plain.iter().enumerate() {
            noted.push(event.clone());
            noted.push(note(n));
        }
        let kept_text = |events: &[SessionEvent], cut: usize| -> Vec<String> {
            events[..cut]
                .iter()
                .filter(|e| !matches!(e, SessionEvent::Progress { .. }))
                .map(|e| format!("{e:?}"))
                .collect()
        };
        for turn in [None, Some(1), Some(2), Some(3)] {
            let (cut, turns) = fork_cut(&plain, turn).unwrap();
            let (noted_cut, noted_turns) = fork_cut(&noted, turn).unwrap();
            assert_eq!(noted_turns, turns, "turn count for {turn:?}");
            assert_eq!(
                kept_text(&noted, noted_cut),
                kept_text(&plain, cut),
                "kept conversation for {turn:?}"
            );
        }
    }

    #[test]
    fn tip_keeps_everything_and_counts_all_turns() {
        let events = three_turns();
        assert_eq!(fork_cut(&events, None).unwrap(), (events.len(), 3));
    }

    #[test]
    fn fork_at_turn_keeps_that_turns_answer_and_stops_before_the_next_task() {
        let events = three_turns();
        // After turn 1: the first task and its answer, nothing from turn 2.
        assert_eq!(fork_cut(&events, Some(1)).unwrap(), (2, 1));
        // After turn 2: the checkpoint belongs to turn 2 and comes along.
        assert_eq!(fork_cut(&events, Some(2)).unwrap(), (5, 2));
        // After the last turn: the whole log.
        assert_eq!(fork_cut(&events, Some(3)).unwrap(), (events.len(), 3));
    }

    #[test]
    fn turn_zero_and_past_the_end_are_refused_with_plain_messages() {
        let events = three_turns();
        let zero = fork_cut(&events, Some(0)).unwrap_err().to_string();
        assert!(zero.contains("numbered from 1"), "{zero}");
        let past = fork_cut(&events, Some(4)).unwrap_err().to_string();
        assert!(past.contains("turn 4 is out of range"), "{past}");
        assert!(past.contains("3 user turns"), "{past}");
    }

    #[test]
    fn a_session_without_turns_has_no_turn_to_fork_at() {
        let events = vec![reply("orphan")];
        assert_eq!(fork_cut(&events, None).unwrap(), (1, 0));
        let err = fork_cut(&events, Some(1)).unwrap_err().to_string();
        assert!(err.contains("no user turns"), "{err}");
    }
}
