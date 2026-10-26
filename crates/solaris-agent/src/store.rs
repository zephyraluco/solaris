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
    pub model: String,
    pub status: String,
    pub updated_at: i64,
}

/// One durable, ordered state transition.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionRecord {
    Prompt { text: String },
    Event { event: AgentEvent },
    Context { messages: Vec<Message> },
    Message { message: Message },
    ToolIntent { call: solaris_backend::ToolCall },
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
        let connection = Connection::open(path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > 1 {
            return Err(StoreError::Invalid(format!(
                "unsupported session schema version {version}"
            )));
        }
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;
            BEGIN IMMEDIATE;
            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY, cwd TEXT NOT NULL, parent_id TEXT REFERENCES sessions(id),
                model TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'idle',
                updated_at INTEGER NOT NULL DEFAULT (unixepoch()));
            CREATE TABLE IF NOT EXISTS records (
                seq INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL REFERENCES sessions(id),
                data TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS records_session ON records(session_id, seq);
            CREATE TABLE IF NOT EXISTS contexts (session_id TEXT PRIMARY KEY REFERENCES sessions(id), data TEXT NOT NULL);
            PRAGMA user_version=1; COMMIT;")?;
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

    /// Create a session with an unguessable local identifier.
    pub fn create(
        self: &Arc<Self>,
        cwd: &Path,
        model: &str,
        parent: Option<&str>,
    ) -> Result<Arc<SessionJournal>, StoreError> {
        let id: String =
            self.lock()?
                .query_row("SELECT lower(hex(randomblob(16)))", [], |row| row.get(0))?;
        self.lock()?.execute(
            "INSERT INTO sessions(id,cwd,model,parent_id) VALUES (?1,?2,?3,?4)",
            params![id, cwd.to_string_lossy(), model, parent],
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
        let mut statement = connection.prepare("SELECT id,cwd,parent_id,model,status,updated_at FROM sessions WHERE cwd=?1 ORDER BY updated_at DESC, rowid DESC")?;
        let rows = statement.query_map([cwd.to_string_lossy()], |row| {
            Ok(SessionInfo {
                id: row.get(0)?,
                cwd: PathBuf::from(row.get::<_, String>(1)?),
                parent_id: row.get(2)?,
                model: row.get(3)?,
                status: row.get(4)?,
                updated_at: row.get(5)?,
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
        data.iter()
            .map(|text| Ok(serde_json::from_str(text)?))
            .collect()
    }
}

/// An execution lease and durable journal for exactly one session.
pub struct SessionJournal {
    store: Arc<SessionStore>,
    id: String,
    _lease: File,
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
        for record in self.store.records(&self.id)? {
            child.append(&record)?;
        }
        child.append(&SessionRecord::Context {
            messages: self.context()?,
        })?;
        Ok(child)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

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
}
