//! Versioned local session journal. Credentials and provider headers never enter this store.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::AgentEvent;
use fs2::FileExt;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use solaris_backend::Message;

/// Storage failures stop execution rather than silently discarding history.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("session database: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("session file: {0}")]
    Io(#[from] std::io::Error),
    #[error("session data: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Invalid(String),
}

/// Searchable metadata; the journal contains the complete transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub cwd: PathBuf,
    pub parent_id: Option<String>,
    #[serde(default)]
    pub is_subagent: bool,
    pub model: String,
    #[serde(default)]
    pub provider: Option<String>,
    pub status: String,
    pub updated_at: i64,
    #[serde(default)]
    pub title: Option<String>,
}

/// One durable, ordered state transition.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionRecord {
    /// Executor lifecycle observations; messages live exclusively in memory.
    Execution {
        task: String,
        detail: serde_json::Value,
    },
    Usage {
        request_id: String,
        usage: solaris_backend::Usage,
        cost: Option<solaris_backend::Cost>,
        kind: crate::UsageKind,
    },
    Model {
        provider: Option<String>,
        model: String,
    },
    Prompt {
        text: String,
    },
    Event {
        event: AgentEvent,
    },
    Cleared,
    ToolIntent {
        call: solaris_backend::ToolCall,
    },
    Interrupted,
}

/// A connection shared by sessions in this process; SQLite serializes other writers.
pub struct SessionStore {
    connection: Mutex<Connection>,
    directory: PathBuf,
}

impl SessionStore {
    /// Open the per-user database, migrating only supported versions.
    pub fn open(path: &Path) -> Result<Arc<Self>, StoreError> {
        let directory = path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
        }
        let mut connection = Connection::open(path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;",
        )?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let version: i64 =
            transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version != 0 && version != 4 {
            return Err(StoreError::Invalid(format!(
                "unsupported session schema version {version}"
            )));
        }
        transaction.execute_batch("CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY, cwd TEXT NOT NULL, parent_id TEXT REFERENCES sessions(id),
                model TEXT NOT NULL, provider TEXT, is_subagent INTEGER NOT NULL DEFAULT 0, status TEXT NOT NULL DEFAULT 'idle',
                updated_at INTEGER NOT NULL DEFAULT (unixepoch()), event_sequence INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS records (
                seq INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL REFERENCES sessions(id), data TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS records_session ON records(session_id, seq);
")?;
        transaction.execute_batch(
            "CREATE UNIQUE INDEX IF NOT EXISTS request_settlements
            ON records(session_id, json_extract(data,'$.request_id'))
            WHERE json_extract(data,'$.type')='usage';
            CREATE TABLE IF NOT EXISTS memory_checkpoints (
                id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id),
                log TEXT NOT NULL, offset INTEGER NOT NULL, length INTEGER NOT NULL,
                digest TEXT NOT NULL, state TEXT NOT NULL);",
        )?;
        transaction.pragma_update(None, "user_version", 4)?;
        transaction.commit()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(Arc::new(Self {
            connection: Mutex::new(connection),
            directory: directory.into(),
        }))
    }

    pub(crate) fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, StoreError> {
        self.connection
            .lock()
            .map_err(|_| StoreError::Invalid("session database lock poisoned".into()))
    }

    /// Create a user session with optional fork lineage and an unguessable ID.
    pub fn create(
        self: &Arc<Self>,
        cwd: &Path,
        model: &str,
        parent: Option<&str>,
    ) -> Result<Arc<SessionJournal>, StoreError> {
        self.create_entry(cwd, model, parent, false)
    }

    /// Create a delegated session excluded from automatic user continuation.
    pub fn create_subagent(
        self: &Arc<Self>,
        cwd: &Path,
        model: &str,
        parent: &str,
    ) -> Result<Arc<SessionJournal>, StoreError> {
        self.create_entry(cwd, model, Some(parent), true)
    }

    fn create_entry(
        self: &Arc<Self>,
        cwd: &Path,
        model: &str,
        parent: Option<&str>,
        is_subagent: bool,
    ) -> Result<Arc<SessionJournal>, StoreError> {
        let id: String =
            self.lock()?
                .query_row("SELECT lower(hex(randomblob(16)))", [], |row| row.get(0))?;
        self.lock()?.execute(
            "INSERT INTO sessions(id,cwd,model,parent_id,is_subagent) VALUES (?1,?2,?3,?4,?5)",
            params![id, cwd.to_string_lossy(), model, parent, is_subagent],
        )?;
        self.resume(&id)
    }

    /// Claim one session exclusively. An OS lock is released even after a crash.
    pub fn resume(self: &Arc<Self>, id: &str) -> Result<Arc<SessionJournal>, StoreError> {
        if id.len() != 32 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(StoreError::Invalid("invalid session ID".into()));
        }
        let mut options = std::fs::OpenOptions::new();
        options.create(true).read(true).write(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lease = options.open(self.directory.join(format!("{id}.lock")))?;
        lease.try_lock_exclusive().map_err(|_| {
            StoreError::Invalid("session is already open in another process".into())
        })?;
        let status: Option<String> = self
            .lock()?
            .query_row("SELECT status FROM sessions WHERE id=?1", [id], |row| {
                row.get(0)
            })
            .optional()?;
        let Some(status) = status else {
            return Err(StoreError::Invalid("session not found".into()));
        };
        let journal = Arc::new(SessionJournal {
            store: Arc::clone(self),
            id: id.into(),
            _lease: lease,
        });
        if status == "running" {
            journal.append(&SessionRecord::Interrupted)?;
        }
        Ok(journal)
    }

    /// Durable location for command output and file before-images.
    pub fn artifact_dir(&self) -> PathBuf {
        self.directory.join("artifacts")
    }

    /// Most recently used sessions for this project, newest first.
    pub fn list(&self, cwd: &Path) -> Result<Vec<SessionInfo>, StoreError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare("SELECT id,cwd,parent_id,model,status,updated_at,provider,(SELECT substr(json_extract(data,'$.text'),1,80) FROM records WHERE session_id=sessions.id AND json_extract(data,'$.type')='prompt' ORDER BY seq LIMIT 1),is_subagent FROM sessions WHERE cwd=?1 ORDER BY updated_at DESC, rowid DESC")?;
        let rows = statement.query_map([cwd.to_string_lossy()], |row| {
            Ok(SessionInfo {
                id: row.get(0)?,
                cwd: PathBuf::from(row.get::<_, String>(1)?),
                parent_id: row.get(2)?,
                model: row.get(3)?,
                status: row.get(4)?,
                updated_at: row.get(5)?,
                provider: row.get(6)?,
                title: row.get(7)?,
                is_subagent: row.get(8)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Read a transcript without acquiring an execution lease.
    pub fn records(&self, id: &str) -> Result<Vec<SessionRecord>, StoreError> {
        let connection = self.lock()?;
        let mut statement =
            connection.prepare("SELECT data FROM records WHERE session_id=?1 ORDER BY seq")?;
        let data = statement
            .query_map([id], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        let mut records = Vec::new();
        for text in data {
            let value: serde_json::Value = serde_json::from_str(&text)?;
            records.push(serde_json::from_value(value)?);
        }
        Ok(records)
    }
}

/// An execution lease and durable journal for exactly one session.
pub struct SessionJournal {
    store: Arc<SessionStore>,
    id: String,
    _lease: File,
}

impl Drop for SessionJournal {
    fn drop(&mut self) {
        // A concurrently spawned process can briefly inherit this open file
        // description before exec closes it. Closing only our descriptor would
        // leave the lease locked until that child finishes exec.
        let _ = fs2::FileExt::unlock(&self._lease);
    }
}

impl SessionJournal {
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn store(&self) -> &Arc<SessionStore> {
        &self.store
    }

    /// Append a product transition; model messages live only in official memory.
    pub fn append(&self, record: &SessionRecord) -> Result<(), StoreError> {
        let data = serde_json::to_string(record)?;
        let mut connection = self.store.lock()?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "INSERT INTO records(session_id,data) VALUES (?1,?2)",
            params![self.id, data],
        )?;
        if let SessionRecord::Model { provider, model } = record {
            transaction.execute(
                "UPDATE sessions SET provider=?2,model=?3 WHERE id=?1",
                params![self.id, provider, model],
            )?;
        }
        let status = match record {
            SessionRecord::Prompt { .. } => Some("running"),
            SessionRecord::Interrupted => Some("interrupted"),
            SessionRecord::Event {
                event: AgentEvent::TurnComplete { .. },
            } => Some("idle"),
            SessionRecord::Event {
                event: AgentEvent::Error(_),
            } => Some("failed"),
            _ => None,
        };
        transaction.execute(
            "UPDATE sessions SET updated_at=unixepoch(), status=coalesce(?2,status) WHERE id=?1",
            params![self.id, status],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn event_sequence(&self) -> Result<u64, StoreError> {
        Ok(self.store.lock()?.query_row(
            "SELECT event_sequence FROM sessions WHERE id=?1",
            [&self.id],
            |row| row.get(0),
        )?)
    }
    pub(crate) fn next_sequence(&self) -> Result<u64, StoreError> {
        Ok(self.store.lock()?.query_row("UPDATE sessions SET event_sequence=event_sequence+1 WHERE id=?1 RETURNING event_sequence", [&self.id], |row| row.get(0))?)
    }
    pub(crate) fn begin_checkpoint(
        &self,
        checkpoint: &crate::checkpoint::PendingCheckpoint,
    ) -> Result<(), StoreError> {
        self.store.lock()?.execute("INSERT INTO memory_checkpoints(id,session_id,log,offset,length,digest,state) VALUES (?1,?2,?3,?4,?5,?6,'pending')",
            params![checkpoint.id, self.id, checkpoint.log, checkpoint.offset, checkpoint.length, checkpoint.digest])?;
        Ok(())
    }
    pub(crate) fn finish_checkpoint(&self, id: &str, state: &str) -> Result<(), StoreError> {
        self.store.lock()?.execute(
            "UPDATE memory_checkpoints SET state=?3 WHERE id=?1 AND session_id=?2",
            params![id, self.id, state],
        )?;
        Ok(())
    }
    pub(crate) fn pending_checkpoints(
        &self,
    ) -> Result<Vec<crate::checkpoint::PendingCheckpoint>, StoreError> {
        let connection = self.store.lock()?;
        let mut query = connection.prepare("SELECT id,log,offset,length,digest FROM memory_checkpoints WHERE session_id=?1 AND state='pending' ORDER BY rowid")?;
        Ok(query
            .query_map([&self.id], |row| {
                Ok(crate::checkpoint::PendingCheckpoint {
                    id: row.get(0)?,
                    log: row.get(1)?,
                    offset: row.get(2)?,
                    length: row.get(3)?,
                    digest: row.get(4)?,
                })
            })?
            .collect::<Result<_, _>>()?)
    }

    pub(crate) fn memory_store(&self) -> Result<rig_memory::DirStore, StoreError> {
        rig_memory::DirStore::open(self.store.directory.join("memory").join(&self.id))
            .map_err(|error| StoreError::Invalid(error.to_string()))
    }
    /// Model history has exactly one source: the official memory store.
    pub fn context(&self) -> Result<Vec<Message>, StoreError> {
        let memory = self.memory_store()?;
        let conversation = rig_memory::load(&memory, "conversation")
            .map_err(|error| StoreError::Invalid(error.to_string()))?;
        Ok(conversation
            .messages()
            .iter()
            .map(|message| Message::from(message.as_ref()))
            .collect())
    }

    /// Fork without copying credentials or sharing the execution lease.
    pub fn fork(&self, cwd: &Path, model: &str) -> Result<Arc<SessionJournal>, StoreError> {
        let child = self.store.create(cwd, model, Some(&self.id))?;
        {
            let mut connection = self.store.lock()?;
            let transaction = connection.transaction()?;
            transaction.execute("INSERT INTO records(session_id,data) SELECT ?1,data FROM records WHERE session_id=?2 ORDER BY seq", params![child.id, self.id])?;
            transaction.commit()?;
        }
        // Fork the native conversation, retaining message identity and summary
        // asides while giving the child its own store and save cursor.
        let original = rig_memory::load(&self.memory_store()?, "conversation")
            .map_err(|error| StoreError::Invalid(error.to_string()))?;
        let destination = crate::checkpoint::CheckpointStore::open(Arc::clone(&child))?;
        let mut forked = rig_memory::load(&destination, "conversation")
            .map_err(|error| StoreError::Invalid(error.to_string()))?;
        let entries = (0..original.messages().len())
            .filter_map(|at| original.entry(at))
            .collect();
        forked.set_entries(entries, rig_core::conversation::Author::plugin("solaris"));
        for (_, aside) in original.asides() {
            forked.add_aside(aside.clone());
        }
        rig_memory::save(&destination, "conversation", &mut forked)
            .map_err(|error| StoreError::Invalid(error.to_string()))?;
        let provider: Option<String> = self.store.lock()?.query_row(
            "SELECT provider FROM sessions WHERE id=?1",
            [&self.id],
            |row| row.get(0),
        )?;
        child.append(&SessionRecord::Model {
            provider,
            model: model.to_string(),
        })?;

        Ok(child)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_memory(journal: &SessionJournal, messages: Vec<Message>) {
        let store = journal.memory_store().unwrap();
        let mut native = rig_memory::load(&store, "conversation").unwrap();
        native.extend(
            messages
                .iter()
                .map(rig_core::message::Message::try_from)
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
        );
        rig_memory::save(&store, "conversation", &mut native).unwrap();
    }
    #[test]
    fn old_sql_context_is_not_imported_into_native_memory() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::open(&directory.path().join("sessions.sqlite")).unwrap();
        let journal = store.create(directory.path(), "test", None).unwrap();
        store
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TABLE contexts (session_id TEXT PRIMARY KEY, data TEXT NOT NULL)",
            )
            .unwrap();
        store
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO contexts VALUES (?1,?2)",
                params![
                    journal.id(),
                    serde_json::to_string(&vec![Message::user("legacy-only")]).unwrap()
                ],
            )
            .unwrap();
        assert!(journal.context().unwrap().is_empty());
        let conversation = crate::conversation::Conversation::new();
        conversation.attach(Arc::clone(&journal)).unwrap();
        assert!(conversation.snapshot().is_empty());
    }
    #[test]
    fn fork_copies_native_messages_and_asides_without_sharing_later_changes() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::open(&directory.path().join("sessions.sqlite")).unwrap();
        let parent = store.create(directory.path(), "test", None).unwrap();
        seed_memory(
            &parent,
            vec![Message::user("one"), Message::assistant("two")],
        );
        let native_store = parent.memory_store().unwrap();
        let mut native = rig_memory::load(&native_store, "conversation").unwrap();
        native.add_aside(
            rig_core::conversation::Aside::new(
                "fixture.summary",
                rig_core::conversation::Author::plugin("test"),
                serde_json::json!({"summary":"keep"}),
            )
            .after(native.id(1)),
        );
        rig_memory::save(&native_store, "conversation", &mut native).unwrap();
        let child = parent.fork(directory.path(), "test").unwrap();
        let child_store = child.memory_store().unwrap();
        let mut copied = rig_memory::load(&child_store, "conversation").unwrap();
        assert_eq!(copied.messages(), native.messages());
        assert_eq!(copied.id(1), native.id(1));
        assert!(
            copied
                .asides()
                .any(|(_, aside)| aside.kind == "fixture.summary" && aside.after == copied.id(1))
        );
        copied.extend([rig_core::message::Message::user("child only")]);
        rig_memory::save(&child_store, "conversation", &mut copied).unwrap();
        assert_eq!(parent.context().unwrap().len(), 2);
        assert_eq!(child.context().unwrap().len(), 3);
    }
    #[test]
    fn dropping_journal_releases_lease_despite_a_duplicated_descriptor() {
        let directory = tempfile::tempdir().expect("directory");
        let store = SessionStore::open(&directory.path().join("sessions.sqlite")).expect("store");
        let journal = store
            .create(directory.path(), "model", None)
            .expect("session");
        let id = journal.id().to_owned();
        let inherited = journal._lease.try_clone().expect("duplicate descriptor");
        assert!(store.resume(&id).is_err());
        drop(journal);
        let resumed = store.resume(&id).expect("released owner lease");
        drop(inherited);
        assert!(
            store.resume(&id).is_err(),
            "new owner must still hold its lock"
        );
        drop(resumed);
    }
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn unsupported_schema_versions_are_rejected_without_migration() {
        for version in [1, 2, 3, 5] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("sessions.sqlite");
            Connection::open(&path)
                .unwrap()
                .pragma_update(None, "user_version", version)
                .unwrap();
            let error = match SessionStore::open(&path) {
                Ok(_) => panic!("old schema accepted"),
                Err(error) => error,
            };
            assert!(error.to_string().contains("unsupported session schema"));
        }
    }

    #[test]
    fn crash_recovery_keeps_history_and_never_replays_tools() {
        let directory = std::env::temp_dir().join(format!(
            "solaris-store-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let store = SessionStore::open(&directory.join("sessions.sqlite")).expect("database");
        let session = store
            .create(Path::new("/project"), "model", None)
            .expect("create");
        let id = session.id().to_string();
        session
            .append(&SessionRecord::Prompt {
                text: "hello".into(),
            })
            .expect("prompt");
        seed_memory(&session, vec![Message::user("hello")]);
        session
            .append(&SessionRecord::ToolIntent {
                call: solaris_backend::ToolCall::new(
                    "call",
                    "bash",
                    serde_json::json!({"command":"echo test"}),
                ),
            })
            .expect("intent");
        assert!(store.resume(&id).is_err(), "second writer must be refused");
        drop(session);
        let session = store.resume(&id).expect("recover");
        assert_eq!(
            session.context().expect("context"),
            vec![Message::user("hello")]
        );
        assert!(matches!(
            store.records(&id).expect("records").last(),
            Some(SessionRecord::Interrupted)
        ));
        let child = session.fork(Path::new("/project"), "model").expect("fork");
        assert_eq!(
            child.context().expect("fork context"),
            session.context().expect("parent context")
        );
        drop(child);
        drop(session);
        drop(store);
        std::fs::remove_dir_all(directory).expect("cleanup");
    }
    #[test]
    fn obsolete_record_formats_are_rejected_instead_of_silently_imported() {
        let directory = tempfile::tempdir().expect("directory");
        let store = SessionStore::open(&directory.path().join("sessions.sqlite")).expect("store");
        let journal = store
            .create(directory.path(), "model", None)
            .expect("session");
        store
            .lock()
            .expect("connection")
            .execute(
                "INSERT INTO records(session_id,data) VALUES (?1,?2)",
                params![
                    journal.id(),
                    r#"{"type":"context_checkpoint","checkpoint":{"unused":true}}"#
                ],
            )
            .expect("old checkpoint");
        journal
            .append(&SessionRecord::Prompt {
                text: "hello".into(),
            })
            .expect("prompt");
        assert!(store.records(journal.id()).is_err());
        assert!(serde_json::from_str::<SessionRecord>(r#"{"type":"auxiliary_usage","usage":{"input_tokens":10,"output_tokens":2},"cost_usd":0.01}"#).is_err(), "legacy charge aliases are not imported");
    }
}
