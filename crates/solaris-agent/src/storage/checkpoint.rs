//! Write-ahead metadata for native memory commits. Never stores or replays messages.
use std::io;
use std::sync::Arc;

use rig_memory::{Files, LogWriteError, Store};
use sha2::{Digest, Sha256};

use crate::{SessionJournal, StoreError};

pub(crate) struct CheckpointStore {
    inner: rig_memory::DirStore,
    journal: Arc<SessionJournal>,
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
impl CheckpointStore {
    pub fn open(journal: Arc<SessionJournal>) -> Result<Self, StoreError> {
        let inner = journal.memory_store()?;
        // A crash between the native write and the journal acknowledgement is
        // resolved from the bytes already present, without executing anything.
        for pending in journal.pending_checkpoints()? {
            let committed = inner.read_log(&pending.log).ok().is_some_and(|bytes| {
                let start = pending.offset as usize;
                let end = start.saturating_add(pending.length as usize);
                bytes
                    .get(start..end)
                    .is_some_and(|bytes| digest(bytes) == pending.digest)
            });
            journal
                .finish_checkpoint(&pending.id, if committed { "committed" } else { "aborted" })?;
        }
        Ok(Self { inner, journal })
    }
    fn write(
        &self,
        name: &str,
        len: u64,
        bytes: &[u8],
        replace: bool,
    ) -> Result<(), LogWriteError> {
        let id = crate::identity::new_id();
        self.journal
            .begin_checkpoint(&PendingCheckpoint {
                id: id.clone(),
                log: name.into(),
                offset: if replace { 0 } else { len },
                length: bytes.len() as u64,
                digest: digest(bytes),
            })
            .map_err(|error| io::Error::other(error.to_string()))?;
        let result = if replace {
            self.inner.replace_log(name, len, bytes)
        } else {
            self.inner.append_log(name, len, bytes)
        };
        self.journal
            .finish_checkpoint(
                &id,
                if result.is_ok() {
                    "committed"
                } else {
                    "aborted"
                },
            )
            .map_err(|error| io::Error::other(error.to_string()))?;
        result
    }
}
pub(crate) struct PendingCheckpoint {
    pub id: String,
    pub log: String,
    pub offset: u64,
    pub length: u64,
    pub digest: String,
}
impl Files for CheckpointStore {
    fn write_file(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        self.inner.write_file(name, bytes)
    }
    fn append_file(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        self.inner.append_file(name, bytes)
    }
    fn read_file(&self, name: &str) -> io::Result<Vec<u8>> {
        self.inner.read_file(name)
    }
    fn remove_file(&self, name: &str) -> io::Result<()> {
        self.inner.remove_file(name)
    }
    fn files(&self) -> io::Result<Vec<String>> {
        self.inner.files()
    }
}
impl Store for CheckpointStore {
    fn logs(&self) -> io::Result<Vec<String>> {
        self.inner.logs()
    }
    fn read_log(&self, name: &str) -> io::Result<Vec<u8>> {
        self.inner.read_log(name)
    }
    fn append_log(&self, name: &str, len: u64, bytes: &[u8]) -> Result<(), LogWriteError> {
        self.write(name, len, bytes, false)
    }
    fn replace_log(&self, name: &str, len: u64, bytes: &[u8]) -> Result<(), LogWriteError> {
        self.write(name, len, bytes, true)
    }
    fn destroy(&self) -> io::Result<()> {
        self.inner.destroy()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recovery_acknowledges_only_written_bytes_and_never_replays_missing_writes() {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::SessionStore::open(&directory.path().join("sessions.sqlite")).unwrap();
        let journal = store.create(directory.path(), "fixture", None).unwrap();
        let memory = journal.memory_store().unwrap();
        memory.append_log("conversation", 0, b"written").unwrap();
        for (id, bytes, offset) in [
            ("committed", b"written".as_slice(), 0),
            ("missing", b"missing".as_slice(), 7),
        ] {
            journal
                .begin_checkpoint(&PendingCheckpoint {
                    id: id.into(),
                    log: "conversation".into(),
                    offset,
                    length: bytes.len() as u64,
                    digest: digest(bytes),
                })
                .unwrap();
        }
        let recovered = CheckpointStore::open(Arc::clone(&journal)).unwrap();
        assert!(journal.pending_checkpoints().unwrap().is_empty());
        assert_eq!(recovered.read_log("conversation").unwrap(), b"written");
        let connection = journal.store().lock().unwrap();
        for (id, expected) in [("committed", "committed"), ("missing", "aborted")] {
            let state: String = connection
                .query_row(
                    "SELECT state FROM memory_checkpoints WHERE id=?1",
                    [id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(state, expected);
        }
    }
}
