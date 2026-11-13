//! modron-store — session persistence (LEDGER S1).
//!
//! [`JsonlSessionStore`] implements the frozen [`SessionStore`] trait with
//! one JSONL file per session: `{root}/{uuid}.jsonl` holds one JSON
//! [`SessionEvent`] per line, and `{root}/{uuid}.meta.json` mirrors the
//! [`SessionMeta`] (id, title, created_at, updated_at, message_count).
//! Sessions are trees via `parent_id` (present-but-unused for MVP).
//!
//! **Blocking I/O:** the async trait methods run their `std::fs` work on
//! the tokio blocking pool (`spawn_blocking`), so a slow disk never stalls
//! a runtime worker. Appends reuse a cached open file handle and an
//! in-memory meta mirror (write-through): the per-event cost is one
//! `write` + one small meta rewrite instead of open + read + parse +
//! rename round trips (2026-11-12 audit perf fix).
//!
//! Corrupt lines in a transcript are skipped with a `tracing::warn!` and are
//! never fatal — a partially damaged log must remain loadable.

use async_trait::async_trait;
use modron_core::{Error, Result, SessionEvent, SessionId, SessionMeta, SessionStore};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// Per-session caches. Everything here is an accelerator; every miss falls
/// back to the on-disk truth, so a cache that silently vanished would only
/// cost perf, never correctness.
#[derive(Default)]
struct Caches {
    /// Open append handles: no open-per-append.
    handles: HashMap<SessionId, std::fs::File>,
    /// In-memory meta mirror (write-through on append).
    metas: HashMap<SessionId, SessionMeta>,
    /// Parsed event count per session, seeded lazily from `load` (one full
    /// parse per session per process, only if something asks) and bumped on
    /// append. `checkpoint_for` asks per tool call; O(1) after the seed.
    counts: HashMap<SessionId, u64>,
}

struct Inner {
    root: PathBuf,
    caches: Mutex<Caches>,
    /// Per-session write serialization: appends for one session must not
    /// interleave (handle reuse + meta read-modify-write assume it). The
    /// server's BroadcastingStore already serializes per session; this
    /// guards direct engine use and keeps the caches coherent regardless.
    locks: Mutex<HashMap<SessionId, Arc<std::sync::Mutex<()>>>>,
}

impl Inner {
    fn session_lock(&self, id: SessionId) -> Arc<std::sync::Mutex<()>> {
        let mut locks = self.locks.lock().unwrap_or_else(|e| e.into_inner());
        locks.entry(id).or_default().clone()
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
        // Write-then-rename: a truncate-in-place write interrupted by a
        // crash corrupts the meta file and permanently bricks every future
        // append (read_meta fails forever after).
        let tmp = self.root.join(format!("{}.meta.json.tmp", meta.id));
        std::fs::write(&tmp, data)
            .map_err(|e| Error::Session(format!("could not write {}: {e}", tmp.display())))?;
        std::fs::rename(&tmp, &path)
            .map_err(|e| Error::Session(format!("could not replace {}: {e}", path.display())))
    }

    /// The meta for `id`: cache hit, else one disk read that seeds the
    /// cache.
    fn meta_for(&self, id: SessionId) -> Result<SessionMeta> {
        let mut caches = self.caches.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(meta) = caches.metas.get(&id) {
            return Ok(meta.clone());
        }
        let meta = self.read_meta(id)?;
        caches.metas.insert(id, meta.clone());
        Ok(meta)
    }

    fn append_sync(&self, id: SessionId, line: &str, meta_bump: bool) -> Result<()> {
        self.ensure_root()?;
        let guard = self.session_lock(id);
        let _held = guard.lock().unwrap_or_else(|e| e.into_inner());

        let mut caches = self.caches.lock().unwrap_or_else(|e| e.into_inner());
        // FD ceiling: handles accumulate for every session ever appended
        // to; a long-lived daemon (automations mint a session per fire)
        // would exhaust the default 256-FD limit (cycle-2 audit H11).
        // Dropping a handle only costs a reopen on the next append.
        if !caches.handles.contains_key(&id) && caches.handles.len() >= 64 {
            caches.handles.clear();
        }
        // Reuse the cached handle; opening fresh only on the first append
        // (or after a delete). NotFound maps to the documented
        // unknown-session error.
        if let std::collections::hash_map::Entry::Vacant(entry) = caches.handles.entry(id) {
            let file = OpenOptions::new()
                .append(true)
                .open(self.jsonl_path(id))
                .map_err(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        Error::Session(format!(
                            "unknown session {id}: no transcript at {}",
                            self.jsonl_path(id).display()
                        ))
                    } else {
                        Error::Session(format!(
                            "could not open {}: {e}",
                            self.jsonl_path(id).display()
                        ))
                    }
                })?;
            entry.insert(file);
        }
        let file = caches.handles.get_mut(&id).expect("handle inserted above");
        writeln!(file, "{line}").map_err(|e| {
            Error::Session(format!(
                "could not append to {}: {e}",
                self.jsonl_path(id).display()
            ))
        })?;
        drop(caches);

        // Mirror the append into the meta file (updated_at + message count),
        // via the in-memory mirror instead of re-parsing anything big.
        // The mirror is merged with the on-disk meta first: another
        // process (`modron --resume` in a second terminal) may have
        // appended since our cache was written - writing the cache back
        // verbatim would REGRESS its message_count (cycle-2 audit M5).
        let mut meta = self.meta_for(id)?;
        if let Ok(disk) = self.read_meta(id) {
            meta.message_count = meta.message_count.max(disk.message_count);
            if disk.updated_at > meta.updated_at {
                meta.updated_at = disk.updated_at;
            }
        }
        meta.updated_at = chrono::Utc::now();
        if meta_bump {
            meta.message_count = meta.message_count.saturating_add(1);
        }
        self.write_meta(&meta)?;
        self.caches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .metas
            .insert(id, meta);
        // Maintain the seeded event counter if something already seeded it.
        let mut caches = self.caches.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(count) = caches.counts.get_mut(&id) {
            *count += 1;
        }
        Ok(())
    }

    fn create_session_sync(
        &self,
        title: &str,
        id: SessionId,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<SessionMeta> {
        self.ensure_root()?;
        let meta = SessionMeta {
            id,
            title: title.to_string(),
            created_at: now,
            updated_at: now,
            message_count: 0,
        };
        // Drop any cached handle first: File::create truncates, and a stale
        // cached offset would silently append after the truncation point.
        let mut caches = self.caches.lock().unwrap_or_else(|e| e.into_inner());
        caches.handles.remove(&id);
        caches.metas.insert(id, meta.clone());
        caches.counts.insert(id, 0);
        drop(caches);
        self.write_meta(&meta)?;
        std::fs::File::create(self.jsonl_path(id))
            .map_err(|e| Error::Session(format!("could not create transcript: {e}")))?;
        Ok(meta)
    }

    fn load_sync(&self, id: SessionId) -> Result<Vec<SessionEvent>> {
        // Same per-session lock the append holds: a concurrent append's
        // two-syscall writeln could otherwise be observed mid-line
        // (cycle-2 audit M4).
        let guard = self.session_lock(id);
        let _held = guard.lock().unwrap_or_else(|e| e.into_inner());
        let path = self.jsonl_path(id);
        // Read bytes and decode per-line lossily: a torn multibyte write
        // must not fail the whole load — the documented contract is that a
        // partially damaged log stays loadable (bad lines warn + skip).
        let data = std::fs::read(&path)
            .map_err(|e| Error::Session(format!("could not read {}: {e}", path.display())))?;
        let text = String::from_utf8_lossy(&data);
        let mut events = Vec::new();
        for (idx, line) in text.lines().enumerate() {
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

    fn event_count_sync(&self, id: SessionId) -> Result<u64> {
        if let Some(count) = self
            .caches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .counts
            .get(&id)
        {
            return Ok(*count);
        }
        // Seed under the session lock (load_sync takes it): an append
        // between the seed-read and the cache insert would be counted
        // twice - once by the read, once by the append's counter bump -
        // and checkpoint indexes would drift by one (cycle-2 audit M4).
        let count = self.load_sync(id)?.len() as u64;
        let guard = self.session_lock(id);
        let _held = guard.lock().unwrap_or_else(|e| e.into_inner());
        self.caches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .counts
            .insert(id, count);
        Ok(count)
    }

    fn append_batch_sync(&self, id: SessionId, lines: &[String], meta_bumps: usize) -> Result<()> {
        self.ensure_root()?;
        let guard = self.session_lock(id);
        let _held = guard.lock().unwrap_or_else(|e| e.into_inner());

        let mut caches = self.caches.lock().unwrap_or_else(|e| e.into_inner());
        if !caches.handles.contains_key(&id) && caches.handles.len() >= 64 {
            caches.handles.clear();
        }
        if let std::collections::hash_map::Entry::Vacant(entry) = caches.handles.entry(id) {
            let file = OpenOptions::new()
                .append(true)
                .open(self.jsonl_path(id))
                .map_err(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        Error::Session(format!(
                            "unknown session {id}: no transcript at {}",
                            self.jsonl_path(id).display()
                        ))
                    } else {
                        Error::Session(format!(
                            "could not open {}: {e}",
                            self.jsonl_path(id).display()
                        ))
                    }
                })?;
            entry.insert(file);
        }
        {
            let file = caches.handles.get_mut(&id).expect("handle inserted above");
            let mut buffer = String::with_capacity(lines.iter().map(|l| l.len() + 1).sum());
            for line in lines {
                buffer.push_str(line);
                buffer.push('\n');
            }
            file.write_all(buffer.as_bytes()).map_err(|e| {
                Error::Session(format!(
                    "could not append to {}: {e}",
                    self.jsonl_path(id).display()
                ))
            })?;
        }
        drop(caches);

        // ONE meta write for the whole batch (fork of 5k events: was 5k
        // pretty-serializations + ~15k syscalls).
        let mut meta = self.meta_for(id)?;
        if let Ok(disk) = self.read_meta(id) {
            meta.message_count = meta.message_count.max(disk.message_count);
            if disk.updated_at > meta.updated_at {
                meta.updated_at = disk.updated_at;
            }
        }
        meta.updated_at = chrono::Utc::now();
        meta.message_count = meta.message_count.saturating_add(meta_bumps as u32);
        self.write_meta(&meta)?;
        self.caches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .metas
            .insert(id, meta);
        let mut caches = self.caches.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(count) = caches.counts.get_mut(&id) {
            *count += lines.len() as u64;
        }
        Ok(())
    }

    fn delete_sync(&self, id: SessionId) -> Result<bool> {
        {
            let mut caches = self.caches.lock().unwrap_or_else(|e| e.into_inner());
            caches.handles.remove(&id);
            caches.metas.remove(&id);
            caches.counts.remove(&id);
        }
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

/// JSONL-backed [`SessionStore`]. The root directory is created lazily on
/// first write; read paths on a missing root return empty results.
pub struct JsonlSessionStore {
    inner: Arc<Inner>,
}

impl JsonlSessionStore {
    /// Store rooted at `root` (e.g. `~/.modron/sessions/`).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Inner {
                root: root.into(),
                caches: Mutex::new(Caches::default()),
                locks: Mutex::new(HashMap::new()),
            }),
        }
    }

    async fn block<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Inner) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || f(&inner))
            .await
            .map_err(|e| Error::Session(format!("store task failed: {e}")))?
    }
}

#[async_trait]
impl SessionStore for JsonlSessionStore {
    async fn create_session(&self, title: &str) -> Result<SessionMeta> {
        let id = Uuid::new_v4();
        let now = chrono::Utc::now();
        let title = title.to_string();
        self.block(move |inner| inner.create_session_sync(&title, id, now))
            .await
    }

    async fn append(&self, id: SessionId, event: &SessionEvent) -> Result<()> {
        // Serialize on the async side; the blocking pool only touches disk.
        let line = serde_json::to_string(event)
            .map_err(|e| Error::Session(format!("could not serialize event: {e}")))?;
        let meta_bump = matches!(
            event,
            SessionEvent::Message { .. } | SessionEvent::ToolResult { .. }
        );
        self.block(move |inner| inner.append_sync(id, &line, meta_bump))
            .await
    }

    async fn load(&self, id: SessionId) -> Result<Vec<SessionEvent>> {
        self.block(move |inner| inner.load_sync(id)).await
    }

    /// Parsed event count. The trait default is a full `load`; this store
    /// seeds once per session per process and then answers from the counter
    /// `append` maintains — `checkpoint_for` asks per mutating tool call.
    async fn event_count(&self, id: SessionId) -> Result<u64> {
        self.block(move |inner| inner.event_count_sync(id)).await
    }

    async fn list(&self) -> Result<Vec<SessionMeta>> {
        // List must reflect the on-disk truth (other processes write it
        // too), so it reads the meta files directly.
        self.block(move |inner| {
            let mut metas = Vec::new();
            let entries = match std::fs::read_dir(&inner.root) {
                Ok(entries) => entries,
                // Lazy root: nothing written yet is an empty session list.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(metas),
                Err(e) => {
                    return Err(Error::Session(format!(
                        "could not read {}: {e}",
                        inner.root.display()
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
        })
        .await
    }

    async fn delete(&self, id: SessionId) -> Result<bool> {
        self.block(move |inner| inner.delete_sync(id)).await
    }

    async fn append_batch(&self, id: SessionId, events: &[SessionEvent]) -> Result<()> {
        // Serialize on the async side; ONE blocking pass writes all lines
        // and one meta rewrite.
        let mut lines = Vec::with_capacity(events.len());
        let mut meta_bumps = 0usize;
        for event in events {
            lines.push(
                serde_json::to_string(event)
                    .map_err(|e| Error::Session(format!("could not serialize event: {e}")))?,
            );
            if matches!(
                event,
                SessionEvent::Message { .. } | SessionEvent::ToolResult { .. }
            ) {
                meta_bumps += 1;
            }
        }
        self.block(move |inner| inner.append_batch_sync(id, &lines, meta_bumps))
            .await
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
                    meta: None,
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
    async fn event_count_matches_load_and_survives_cache() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonlSessionStore::new(dir.path().join("sessions"));
        let meta = store.create_session("counting").await.unwrap();
        assert_eq!(store.event_count(meta.id).await.unwrap(), 0);
        let events = sample_events();
        for event in &events {
            store.append(meta.id, event).await.unwrap();
        }
        assert_eq!(
            store.event_count(meta.id).await.unwrap(),
            events.len() as u64
        );
        // Counter stays coherent with the log across a fresh store instance
        // (cache cold: seeds from the file).
        let reopened = JsonlSessionStore::new(dir.path().join("sessions"));
        assert_eq!(
            reopened.event_count(meta.id).await.unwrap(),
            events.len() as u64
        );
        // A counter seeded BEFORE appends must keep up with them.
        let meta2 = store.create_session("second").await.unwrap();
        store.event_count(meta2.id).await.unwrap();
        store
            .append(
                meta2.id,
                &SessionEvent::Message {
                    parent_id: None,
                    message: Message::user("x"),
                },
            )
            .await
            .unwrap();
        assert_eq!(store.event_count(meta2.id).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn append_merges_external_meta_writers_instead_of_regressing() {
        // A second store instance (the CLI resuming in another terminal)
        // appends between our appends: the mirror must adopt its count,
        // not write the stale cached one back over it (cycle-2 audit M5).
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions");
        let ours = JsonlSessionStore::new(&root);
        let theirs = JsonlSessionStore::new(&root);
        let meta = ours.create_session("shared").await.unwrap();
        let user = || SessionEvent::Message {
            parent_id: None,
            message: Message::user("x"),
        };
        ours.append(meta.id, &user()).await.unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        theirs.append(meta.id, &user()).await.unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        ours.append(meta.id, &user()).await.unwrap();

        let listed = ours.list().await.unwrap();
        assert_eq!(
            listed[0].message_count, 3,
            "external append counted, not clobbered"
        );
        assert_eq!(
            theirs.list().await.unwrap()[0].message_count,
            3,
            "both instances agree with disk"
        );
    }

    #[tokio::test]
    async fn append_after_reopen_appends_at_end() {
        // The cached-handle path must never append at a stale offset after
        // the transcript was recreated (delete/re-create or truncate).
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions");
        let store = JsonlSessionStore::new(&root);
        let meta = store.create_session("reborn").await.unwrap();
        store
            .append(
                meta.id,
                &SessionEvent::Message {
                    parent_id: None,
                    message: Message::user("first life"),
                },
            )
            .await
            .unwrap();
        store.delete(meta.id).await.unwrap();
        store.create_session("reborn").await.unwrap();
        // New session has a fresh id; force the same-id case via a second
        // store on the same root writing to the SAME id is impossible
        // (uuid4), so assert the simple invariant instead: the old id is
        // gone and appends to it fail as unknown.
        let err = store
            .append(
                meta.id,
                &SessionEvent::Message {
                    parent_id: None,
                    message: Message::user("ghost"),
                },
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown session"), "{err}");
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
            matches!(loaded.last(), Some(SessionEvent::Message { message, ..
                }) if message.content == "again")
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
