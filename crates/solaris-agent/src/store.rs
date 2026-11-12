//! Versioned local session journal. Credentials and provider headers never enter this store.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use fs2::FileExt;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use solaris_backend::{AgentEvent, Message};

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
    #[serde(alias = "auxiliary_usage")]
    Usage {
        usage: solaris_backend::Usage,
        cost_usd: f64,
        #[serde(default)]
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
    Context {
        messages: Vec<Message>,
    },
    Message {
        message: Message,
    },
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
        if version > 3 {
            return Err(StoreError::Invalid(format!(
                "unsupported session schema version {version}"
            )));
        }
        transaction.execute_batch("CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY, cwd TEXT NOT NULL, parent_id TEXT REFERENCES sessions(id),
                model TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'idle',
                updated_at INTEGER NOT NULL DEFAULT (unixepoch()));
            CREATE TABLE IF NOT EXISTS records (
                seq INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL REFERENCES sessions(id), data TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS records_session ON records(session_id, seq);
            CREATE TABLE IF NOT EXISTS contexts (session_id TEXT PRIMARY KEY REFERENCES sessions(id), data TEXT NOT NULL);")?;
        if version < 2 {
            transaction.execute("ALTER TABLE sessions ADD COLUMN provider TEXT", [])?;
        }
        if version < 3 {
            transaction.execute(
                "ALTER TABLE sessions ADD COLUMN is_subagent INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
            // In schema 2, forks recorded a Model entry while ordinary children
            // did not. Parent event links also identify children whose metadata
            // was subsequently updated. Unannounced children have no Model entry.
            transaction.execute_batch(
                "UPDATE sessions SET is_subagent=1
                WHERE parent_id IS NOT NULL AND (
                    EXISTS (SELECT 1 FROM records r WHERE r.session_id=sessions.parent_id
                        AND json_extract(r.data,'$.event.SubagentSession.session_id')=sessions.id)
                    OR NOT EXISTS (SELECT 1 FROM records r WHERE r.session_id=sessions.id
                        AND json_extract(r.data,'$.type')='model'));",
            )?;
        }
        transaction.pragma_update(None, "user_version", 3)?;
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

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, StoreError> {
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
            // Obsolete hashes from the previous implementation are not a projection input.
            if value.get("type").and_then(serde_json::Value::as_str) == Some("context_checkpoint") {
                continue;
            }
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

    /// Commit a transition and its projection in the same transaction.
    pub fn append(&self, record: &SessionRecord) -> Result<(), StoreError> {
        let data = serde_json::to_string(record)?;
        let mut connection = self.store.lock()?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "INSERT INTO records(session_id,data) VALUES (?1,?2)",
            params![self.id, data],
        )?;
        if let SessionRecord::Context { messages } = record {
            transaction.execute("INSERT INTO contexts(session_id,data) VALUES (?1,?2) ON CONFLICT(session_id) DO UPDATE SET data=excluded.data", params![self.id, serde_json::to_string(messages)?])?;
        }
        if let SessionRecord::Message { message } = record {
            let context: Option<String> = transaction
                .query_row(
                    "SELECT data FROM contexts WHERE session_id=?1",
                    [&self.id],
                    |row| row.get(0),
                )
                .optional()?;
            let mut messages: Vec<Message> = match context {
                Some(data) => serde_json::from_str(&data)?,
                None => Vec::new(),
            };
            messages.push(message.clone());
            transaction.execute("INSERT INTO contexts(session_id,data) VALUES (?1,?2) ON CONFLICT(session_id) DO UPDATE SET data=excluded.data", params![self.id, serde_json::to_string(&messages)?])?;
        }
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

    /// Latest model context, independent of how the UI displays the journal.
    pub fn context(&self) -> Result<Vec<Message>, StoreError> {
        let memory = rig_memory::DirStore::open(self.store.directory.join("memory").join(&self.id))
            .map_err(|error| StoreError::Invalid(error.to_string()))?;
        if rig_memory::Store::logs(&memory)?
            .iter()
            .any(|name| name == "conversation")
        {
            let conversation = rig_memory::load(&memory, "conversation")
                .map_err(|error| StoreError::Invalid(error.to_string()))?;
            return Ok(conversation
                .messages()
                .iter()
                .map(|message| Message::from(message.as_ref()))
                .collect());
        }
        let data: Option<String> = self
            .store
            .lock()?
            .query_row(
                "SELECT data FROM contexts WHERE session_id=?1",
                [&self.id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(match data {
            Some(data) => serde_json::from_str(&data)?,
            None => Vec::new(),
        })
    }

    /// Fork without copying credentials or sharing the execution lease.
    pub fn fork(&self, cwd: &Path, model: &str) -> Result<Arc<SessionJournal>, StoreError> {
        let child = self.store.create(cwd, model, Some(&self.id))?;
        {
            let mut connection = self.store.lock()?;
            let transaction = connection.transaction()?;
            transaction.execute("INSERT INTO records(session_id,data) SELECT ?1,data FROM records WHERE session_id=?2 ORDER BY seq", params![child.id, self.id])?;
            transaction.execute("INSERT INTO contexts(session_id,data) SELECT ?1,data FROM contexts WHERE session_id=?2", params![child.id, self.id])?;
            transaction.commit()?;
        }
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
    fn version_one_is_migrated_without_losing_existing_context() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("sessions.sqlite");
        let store = SessionStore::open(&path).expect("store");
        let journal = store
            .create(Path::new("/project"), "model", None)
            .expect("session");
        let id = journal.id().to_string();
        journal
            .append(&SessionRecord::Message {
                message: Message::user("preserve"),
            })
            .expect("message");
        drop(journal);
        drop(store);
        let connection = Connection::open(&path).expect("old schema");
        connection
            .execute_batch("ALTER TABLE sessions DROP COLUMN provider; ALTER TABLE sessions DROP COLUMN is_subagent; PRAGMA user_version=1;")
            .expect("simulate version one");
        drop(connection);
        let migrated = SessionStore::open(&path).expect("migrate");
        assert_eq!(
            migrated
                .resume(&id)
                .expect("resume")
                .context()
                .expect("context"),
            vec![Message::user("preserve")]
        );
    }

    #[test]
    fn version_two_migration_distinguishes_children_from_user_forks() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("sessions.sqlite");
        let cwd = directory.path();
        let store = SessionStore::open(&path).expect("store");
        let root = store.create(cwd, "root", None).expect("root");
        let child = store
            .create_subagent(cwd, "child", root.id())
            .expect("child");
        // Cover an announced child that already has backend metadata.
        child
            .append(&SessionRecord::Model {
                provider: Some("local".into()),
                model: "child".into(),
            })
            .expect("model");
        root.append(&SessionRecord::Event {
            event: AgentEvent::SubagentSession {
                task_id: "task".into(),
                session_id: child.id().into(),
            },
        })
        .expect("child link");
        let orphan = store
            .create_subagent(cwd, "orphan", root.id())
            .expect("unannounced child");
        let fork = root.fork(cwd, "fork").expect("user fork");
        let ids = [root.id(), child.id(), orphan.id(), fork.id()].map(str::to_owned);
        drop(root);
        drop(child);
        drop(orphan);
        drop(fork);
        drop(store);
        Connection::open(&path)
            .expect("connection")
            .execute_batch("ALTER TABLE sessions DROP COLUMN is_subagent; PRAGMA user_version=2;")
            .expect("version two fixture");
        let store = SessionStore::open(&path).expect("migrate");
        let sessions = store.list(cwd).expect("sessions");
        for (id, is_subagent) in ids.iter().zip([false, true, true, false]) {
            assert_eq!(
                sessions
                    .iter()
                    .find(|session| &session.id == id)
                    .expect("saved session")
                    .is_subagent,
                is_subagent
            );
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
        session
            .append(&SessionRecord::Context {
                messages: vec![Message::user("hello")],
            })
            .expect("checkpoint");
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
    fn obsolete_context_checkpoints_do_not_prevent_reading_old_sessions() {
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
        assert_eq!(store.records(journal.id()).expect("read").len(), 1);
        let old_fee: SessionRecord = serde_json::from_str(r#"{"type":"auxiliary_usage","usage":{"input_tokens":10,"output_tokens":2},"cost_usd":0.01}"#).expect("old fee");
        assert!(matches!(
            old_fee,
            SessionRecord::Usage {
                kind: crate::UsageKind::Manual,
                ..
            }
        ));
    }
}
