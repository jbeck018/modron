//! modron-store — session persistence (LEDGER S1).
//!
//! [`JsonlSessionStore`] implements the frozen [`SessionStore`] trait with
//! one JSONL file per session: `{root}/{uuid}.jsonl` holds one JSON
//! [`SessionEvent`] per line, and `{root}/{uuid}.meta.json` mirrors the
//! [`SessionMeta`] (id, title, created_at, updated_at, message_count).
//! Sessions are trees via `parent_id` (present-but-unused for MVP).
//!
//! **Blocking I/O note:** the async trait methods use `std::fs` directly.
//! Local metadata-sized reads/writes make this acceptable for MVP; a future
//! store (libsql, per PLAN.md §4.5) will replace the implementation, not the
//! trait.
//!
//! Corrupt lines in a transcript are skipped with a `tracing::warn!` and are
//! never fatal — a partially damaged log must remain loadable.

use async_trait::async_trait;
use modron_core::{Error, Result, SessionEvent, SessionId, SessionMeta, SessionStore};
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::PathBuf;
use uuid::Uuid;

/// JSONL-backed [`SessionStore`]. The root directory is created lazily on
/// first write; read paths on a missing root return empty results.
pub struct JsonlSessionStore {
    root: PathBuf,
}

impl JsonlSessionStore {
    /// Store rooted at `root` (e.g. `~/.modron/sessions/`).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn jsonl_path(&self, id: SessionId) -> PathBuf {
        self.root.join(format!("{id}.jsonl"))
    }

    fn meta_path(&self, id: SessionId) -> PathBuf {
        self.root.join(format!("{id}.meta.json"))
    }

    fn ensure_root(&self) -> Result<()> {
        std::fs::create_dir_all(&self.root)
            .map_err(|e| Error::Session(format!("could not create {}: {e}", self.root.display())))
    }

    fn read_meta(&self, id: SessionId) -> Result<SessionMeta> {
        let path = self.meta_path(id);
        let data = std::fs::read_to_string(&path)
            .map_err(|e| Error::Session(format!("could not read {}: {e}", path.display())))?;
        serde_json::from_str(&data)
            .map_err(|e| Error::Session(format!("corrupt meta {}: {e}", path.display())))
    }

    fn write_meta(&self, meta: &SessionMeta) -> Result<()> {
        let path = self.meta_path(meta.id);
        let data = serde_json::to_string_pretty(meta)
            .map_err(|e| Error::Session(format!("could not serialize meta: {e}")))?;
        std::fs::write(&path, data)
            .map_err(|e| Error::Session(format!("could not write {}: {e}", path.display())))
    }
}

#[async_trait]
impl SessionStore for JsonlSessionStore {
    async fn create_session(&self, title: &str) -> Result<SessionMeta> {
        self.ensure_root()?;
        let now = chrono::Utc::now();
        let meta = SessionMeta {
            id: Uuid::new_v4(),
            title: title.to_string(),
            created_at: now,
            updated_at: now,
            message_count: 0,
        };
        let data = serde_json::to_string_pretty(&meta)
            .map_err(|e| Error::Session(format!("could not serialize meta: {e}")))?;
        std::fs::write(self.meta_path(meta.id), data)
            .map_err(|e| Error::Session(format!("could not write meta: {e}")))?;
        std::fs::File::create(self.jsonl_path(meta.id))
            .map_err(|e| Error::Session(format!("could not create transcript: {e}")))?;
        Ok(meta)
    }

    async fn append(&self, id: SessionId, event: &SessionEvent) -> Result<()> {
        self.ensure_root()?;
        let path = self.jsonl_path(id);
        if !path.exists() {
            return Err(Error::Session(format!(
                "unknown session {id}: no transcript at {}",
                path.display()
            )));
        }
        let line = serde_json::to_string(event)
            .map_err(|e| Error::Session(format!("could not serialize event: {e}")))?;
        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(|e| Error::Session(format!("could not open {}: {e}", path.display())))?;
        writeln!(file, "{line}")
            .map_err(|e| Error::Session(format!("could not append to {}: {e}", path.display())))?;

        // Mirror the append into the meta file (updated_at + message count).
        let mut meta = self.read_meta(id)?;
        meta.updated_at = chrono::Utc::now();
        if matches!(
            event,
            SessionEvent::Message { .. } | SessionEvent::ToolResult { .. }
        ) {
            meta.message_count = meta.message_count.saturating_add(1);
        }
        self.write_meta(&meta)
    }

    async fn load(&self, id: SessionId) -> Result<Vec<SessionEvent>> {
        let path = self.jsonl_path(id);
        let data = std::fs::read_to_string(&path)
            .map_err(|e| Error::Session(format!("could not read {}: {e}", path.display())))?;
        let mut events = Vec::new();
        for (idx, line) in data.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<SessionEvent>(line) {
                Ok(event) => events.push(event),
                Err(e) => {
                    tracing::warn!(
                        session = %id,
                        line = idx + 1,
                        error = %e,
                        "skipping corrupt line in session transcript"
                    );
                }
            }
        }
        Ok(events)
    }

    async fn list(&self) -> Result<Vec<SessionMeta>> {
        let mut metas = Vec::new();
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            // Lazy root: nothing written yet is an empty session list.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(metas),
            Err(e) => {
                return Err(Error::Session(format!(
                    "could not read {}: {e}",
                    self.root.display()
                )))
            }
        };
        for entry in entries {
            let Ok(entry) = entry else {
                tracing::warn!("skipping unreadable directory entry in session root");
                continue;
            };
            let path = entry.path();
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !name.ends_with(".meta.json") {
                continue;
            }
            let parsed = std::fs::read_to_string(&path)
                .map_err(|e| Error::Session(format!("could not read {name}: {e}")))
                .and_then(|data| {
                    serde_json::from_str::<SessionMeta>(&data)
                        .map_err(|e| Error::Session(format!("corrupt meta {name}: {e}")))
                });
            match parsed {
                Ok(meta) => metas.push(meta),
                Err(e) => {
                    tracing::warn!(file = %name, error = %e, "skipping unreadable session meta")
                }
            }
        }
        metas.sort_by_key(|meta| std::cmp::Reverse(meta.updated_at));
        Ok(metas)
    }

    async fn delete(&self, id: SessionId) -> Result<bool> {
        let base = self.root.join(id.to_string());
        let jsonl = base.with_extension("jsonl");
        let meta = base.with_extension("meta.json");
        let rewind = base.with_extension("rewind");
        let mut existed = false;
        for path in [jsonl, meta, rewind] {
            match std::fs::remove_file(&path) {
                Ok(()) => existed = true,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(Error::Session(format!(
                        "cannot delete {}: {e}",
                        path.display()
                    )))
                }
            }
        }
        Ok(existed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use modron_core::{Message, ToolOutcome, Usage};
    use serde_json::json;

    fn sample_events() -> Vec<SessionEvent> {
        vec![
            SessionEvent::Task {
                text: "add a feature".into(),
            },
            SessionEvent::Message {
                parent_id: None,
                message: Message::user("hello"),
            },
            SessionEvent::ModelSelected {
                model: "m1".into(),
                reason: "default".into(),
                action: None,
                context_tokens: None,
            },
            SessionEvent::Message {
                parent_id: None,
                message: Message::assistant("hi"),
            },
            SessionEvent::ToolResult {
                call_id: "call-1".into(),
                outcome: ToolOutcome {
                    content: "42".into(),
                    is_error: false,
                    usage: None,
                },
            },
            SessionEvent::Usage {
                model: "m1".into(),
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    cached_input_tokens: 0,
                },
            },
        ]
    }

    fn event_json(event: &SessionEvent) -> serde_json::Value {
        serde_json::to_value(event).expect("events serialize")
    }

    #[tokio::test]
    async fn roundtrip_create_append_load() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonlSessionStore::new(dir.path().join("sessions"));

        let meta = store.create_session("roundtrip").await.unwrap();
        for event in sample_events() {
            store.append(meta.id, &event).await.unwrap();
        }
        let loaded = store.load(meta.id).await.unwrap();

        let expected: Vec<SessionEvent> = sample_events();
        assert_eq!(loaded.len(), expected.len(), "event count must roundtrip");
        for (got, want) in loaded.iter().zip(expected.iter()) {
            assert_eq!(event_json(got), event_json(want), "event mismatch");
        }
    }

    #[tokio::test]
    async fn meta_tracks_message_count_and_timestamps() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonlSessionStore::new(dir.path().join("sessions"));
        let meta = store.create_session("counting").await.unwrap();
        assert_eq!(meta.message_count, 0);

        for event in sample_events() {
            store.append(meta.id, &event).await.unwrap();
        }
        // 2 Messages + 1 ToolResult count; Task/ModelSelected/Usage do not.
        let listed = store.list().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, meta.id);
        assert_eq!(listed[0].message_count, 3);
        assert!(listed[0].updated_at >= listed[0].created_at);
        assert_eq!(listed[0].title, "counting");
    }

    #[tokio::test]
    async fn resume_reconstructs_transcript_order() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions");

        let events = sample_events();
        {
            let store = JsonlSessionStore::new(&root);
            let meta = store.create_session("resume me").await.unwrap();
            for event in &events {
                store.append(meta.id, event).await.unwrap();
            }
            // Store dropped here — simulates process exit.
        }
        let reopened = JsonlSessionStore::new(&root);
        let listed = reopened.list().await.unwrap();
        assert_eq!(listed.len(), 1, "resume must find the old session");
        let id = listed[0].id;

        let loaded = reopened.load(id).await.unwrap();
        assert_eq!(loaded.len(), events.len());
        for (got, want) in loaded.iter().zip(events.iter()) {
            assert_eq!(
                event_json(got),
                event_json(want),
                "transcript order/content must match"
            );
        }

        // Appending after resume keeps the log append-only and consistent.
        reopened
            .append(
                id,
                &SessionEvent::Message {
                    parent_id: None,
                    message: Message::user("again"),
                },
            )
            .await
            .unwrap();
        let loaded = reopened.load(id).await.unwrap();
        assert_eq!(loaded.len(), events.len() + 1);
        assert!(
            matches!(loaded.last(), Some(SessionEvent::Message { message, .. }) if message.content == "again")
        );
    }

    #[tokio::test]
    async fn corrupt_line_is_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonlSessionStore::new(dir.path().join("sessions"));
        let meta = store.create_session("corrupt").await.unwrap();
        let events = sample_events();
        for event in &events {
            store.append(meta.id, event).await.unwrap();
        }

        // Splice garbage bytes into the middle of the transcript file.
        let jsonl_path = dir
            .path()
            .join("sessions")
            .join(format!("{}.jsonl", meta.id));
        let data = std::fs::read_to_string(&jsonl_path).unwrap();
        let lines: Vec<&str> = data.lines().collect();
        assert!(lines.len() >= 2);
        let mut spliced = String::new();
        spliced.push_str(lines[0]);
        spliced.push_str("\n{{{{not json at all\n");
        for line in &lines[1..] {
            spliced.push_str(line);
            spliced.push('\n');
        }
        std::fs::write(&jsonl_path, spliced).unwrap();

        let loaded = store.load(meta.id).await.unwrap();
        assert_eq!(
            loaded.len(),
            events.len(),
            "garbage line skipped, good lines kept"
        );
        for (got, want) in loaded.iter().zip(events.iter()) {
            assert_eq!(event_json(got), event_json(want));
        }
    }

    #[tokio::test]
    async fn list_orders_by_updated_at_desc() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonlSessionStore::new(dir.path().join("sessions"));

        let s1 = store.create_session("first").await.unwrap();
        std::thread::sleep(std::time::Duration::from_millis(15));
        let s2 = store.create_session("second").await.unwrap();
        std::thread::sleep(std::time::Duration::from_millis(15));
        let s3 = store.create_session("third").await.unwrap();
        std::thread::sleep(std::time::Duration::from_millis(15));

        // Touch s1 last so it jumps to the front.
        store
            .append(
                s1.id,
                &SessionEvent::Message {
                    parent_id: None,
                    message: Message::user("bump"),
                },
            )
            .await
            .unwrap();

        let listed = store.list().await.unwrap();
        let ids: Vec<SessionId> = listed.iter().map(|m| m.id).collect();
        assert_eq!(ids, vec![s1.id, s3.id, s2.id], "sorted by updated_at desc");
        assert!(listed[0].updated_at >= listed[1].updated_at);
        assert!(listed[1].updated_at >= listed[2].updated_at);
    }

    #[tokio::test]
    async fn unknown_session_errors_are_actionable() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonlSessionStore::new(dir.path().join("sessions"));
        let ghost = Uuid::new_v4();

        let err = store
            .append(ghost, &SessionEvent::Task { text: "x".into() })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown session"), "{err}");

        let err = store.load(ghost).await.unwrap_err();
        assert!(err.to_string().contains("could not read"), "{err}");
    }

    #[tokio::test]
    async fn list_on_missing_root_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonlSessionStore::new(dir.path().join("does-not-exist"));
        assert!(store.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn events_serialize_with_expected_tags() {
        // Guard the wire format: serde tag "type" with snake_case variants.
        let task = json!({"type": "task", "text": "x"});
        let parsed: SessionEvent = serde_json::from_value(task).unwrap();
        assert!(matches!(parsed, SessionEvent::Task { text } if text == "x"));
    }
    #[tokio::test]
    async fn delete_removes_session_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonlSessionStore::new(dir.path());
        let meta = store.create_session("doomed").await.unwrap();
        store
            .append(meta.id, &SessionEvent::Task { text: "t".into() })
            .await
            .unwrap();
        assert!(dir.path().join(format!("{}.jsonl", meta.id)).exists());

        assert!(store.delete(meta.id).await.unwrap());
        assert!(!dir.path().join(format!("{}.jsonl", meta.id)).exists());
        assert!(!dir.path().join(format!("{}.meta.json", meta.id)).exists());
        assert!(
            !store.delete(meta.id).await.unwrap(),
            "second delete = false"
        );
        // Load of a deleted session errors (missing file) — documented.
        assert!(store.load(meta.id).await.is_err());
    }
}
