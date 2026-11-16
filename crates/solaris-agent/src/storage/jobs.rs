//! Durable job snapshots and transactional completion outbox.
use super::*;
use crate::JobInfo;
impl SessionStore {
    pub(crate) fn save_job(&self, info: &JobInfo, notify: bool) -> Result<(), StoreError> {
        let mut connection = self.lock()?;
        let tx = connection.transaction()?;
        tx.execute("INSERT INTO jobs(id,owner,data) VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET data=excluded.data",params![info.id,info.owner_session_id,serde_json::to_string(info)?])?;
        if notify {
            tx.execute(
                "INSERT OR IGNORE INTO inbox(id,owner,delivered) VALUES(?1,?2,0)",
                params![info.id, info.owner_session_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn load_jobs(&self, owner: &str) -> Result<Vec<(JobInfo, bool)>, StoreError> {
        let connection = self.lock()?;
        let mut query=connection.prepare("SELECT jobs.data, coalesce(inbox.delivered,0) FROM jobs LEFT JOIN inbox ON inbox.id=jobs.id WHERE jobs.owner=?1 ORDER BY jobs.rowid")?;
        let rows = query.query_map([owner], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
        })?;
        rows.map(|row| {
            let (data, delivered) = row?;
            Ok((serde_json::from_str(&data)?, delivered))
        })
        .collect()
    }
    pub(crate) fn acknowledge_job(&self, id: &str) -> Result<(), StoreError> {
        self.lock()?
            .execute("UPDATE inbox SET delivered=1 WHERE id=?1", [id])?;
        Ok(())
    }
}

impl SessionStore {
    pub(crate) fn save_input(
        &self,
        input: &crate::runtime::jobs::QueuedInput,
    ) -> Result<(), StoreError> {
        self.lock()?.execute(
            "INSERT INTO inputs(id,owner,data) VALUES(?1,?2,?3)",
            params![input.id, input.owner, serde_json::to_string(input)?],
        )?;
        Ok(())
    }
    pub(crate) fn load_inputs(
        &self,
        owner: &str,
    ) -> Result<Vec<crate::runtime::jobs::QueuedInput>, StoreError> {
        let connection = self.lock()?;
        let mut query = connection
            .prepare("SELECT data FROM inputs WHERE owner=?1 AND delivered=0 ORDER BY rowid")?;
        query
            .query_map([owner], |row| row.get::<_, String>(0))?
            .map(|row| Ok(serde_json::from_str(&row?)?))
            .collect()
    }
    pub(crate) fn acknowledge_input(&self, id: &str) -> Result<(), StoreError> {
        self.lock()?
            .execute("UPDATE inputs SET delivered=1 WHERE id=?1", [id])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{JobStatus, Jobs, StopReason};
    fn job(owner: &str) -> JobInfo {
        JobInfo {
            id: "job-test".into(),
            run_id: "run-test".into(),
            owner_session_id: owner.into(),
            target_session_id: None,
            background: true,
            status: JobStatus::Running,
            started_at: 1,
            finished_at: None,
            stop_reason: None,
            result: None,
        }
    }
    #[test]
    fn queued_inputs_survive_restart_and_are_acknowledged_after_native_commit() {
        use crate::storage::memory::Conversation;
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open(&dir.path().join("sessions.sqlite")).unwrap();
        let journal = store.create(dir.path(), "model", None).unwrap();
        let jobs = Jobs::default();
        jobs.enqueue(journal.id(), "first", false, Some(journal.clone()))
            .unwrap();
        jobs.enqueue(journal.id(), "second", true, Some(journal.clone()))
            .unwrap();
        let restored = Jobs::default();
        restored.restore(&journal).unwrap();
        let pending = restored.pending_inputs(journal.id(), true);
        assert_eq!(
            pending
                .iter()
                .map(|item| item.text.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second"]
        );
        let memory = Conversation::new();
        memory.attach(journal.clone()).unwrap();
        for item in pending {
            memory.push_notification(&item.id, &item.text).unwrap();
            restored.acknowledge_input(&item.id).unwrap();
        }
        let reopened = Jobs::default();
        reopened.restore(&journal).unwrap();
        assert!(reopened.pending_inputs(journal.id(), true).is_empty());
        assert_eq!(memory.len(), 2);
    }
    #[test]
    fn failed_event_sequence_write_also_rolls_back_event_content() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open(&dir.path().join("sessions.sqlite")).unwrap();
        let journal = store.create(dir.path(), "model", None).unwrap();
        store.lock().unwrap().execute_batch("CREATE TRIGGER reject_sequence BEFORE UPDATE OF event_sequence ON sessions BEGIN SELECT RAISE(FAIL,'sequence rejected'); END;").unwrap();
        assert!(
            journal
                .publish_event(&AgentEvent::TextDelta("not committed".into()))
                .is_err()
        );
        assert_eq!(journal.event_sequence().unwrap(), 0);
        assert!(store.records(journal.id()).unwrap().is_empty());
    }

    #[test]
    fn completion_and_outbox_are_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open(&dir.path().join("sessions.sqlite")).unwrap();
        let journal = store.create(dir.path(), "model", None).unwrap();
        let mut info = job(journal.id());
        store.save_job(&info, false).unwrap();
        store.lock().unwrap().execute_batch("CREATE TRIGGER reject_notification BEFORE INSERT ON inbox BEGIN SELECT RAISE(FAIL,'notification rejected'); END;").unwrap();
        info.status = JobStatus::Completed;
        assert!(store.save_job(&info, true).is_err());
        assert_eq!(
            store.load_jobs(journal.id()).unwrap()[0].0.status,
            JobStatus::Running
        );
    }
    #[test]
    fn recovery_marks_running_jobs_interrupted_and_preserves_pending_delivery() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open(&dir.path().join("sessions.sqlite")).unwrap();
        let journal = store.create(dir.path(), "model", None).unwrap();
        store.save_job(&job(journal.id()), false).unwrap();
        let jobs = Jobs::default();
        jobs.restore(&journal).unwrap();
        let info = jobs.get("job-test").unwrap();
        assert_eq!(info.status, JobStatus::Interrupted);
        assert_eq!(info.stop_reason, Some(StopReason::Shutdown));
        assert_eq!(jobs.pending(journal.id()).len(), 1);
        jobs.acknowledge("job-test").unwrap();
        let restored = Jobs::default();
        restored.restore(&journal).unwrap();
        assert!(restored.pending(journal.id()).is_empty());
    }
    #[test]
    fn retry_after_native_commit_does_not_duplicate_notification() {
        use crate::storage::memory::Conversation;
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open(&dir.path().join("sessions.sqlite")).unwrap();
        let journal = store.create(dir.path(), "model", None).unwrap();
        let conversation = Conversation::new();
        conversation.attach(journal.clone()).unwrap();
        conversation
            .push_notification("job-test", "child result")
            .unwrap();
        drop(conversation);
        let restored = Conversation::new();
        restored.attach(journal).unwrap();
        restored
            .push_notification("job-test", "child result")
            .unwrap();
        assert_eq!(restored.len(), 1);
    }
}
