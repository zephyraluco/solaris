//! Runtime-owned executions: foreground and background differ only in result delivery.
use crate::tasks::{SubagentRunner, Task};
use crate::{Agent, AgentEvent, SessionJournal, StoreError, UsageTotals};
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use solaris_tools::{Cancel, ToolContext, ToolOutput};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc::UnboundedSender, watch};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Running,
    Completed,
    Partial,
    Failed,
    Cancelled,
    Interrupted,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Completed,
    StepLimit,
    ExecutionError,
    UserCancelled,
    Shutdown,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobResult {
    pub output: String,
    pub is_error: bool,
    pub metadata: solaris_tools::ToolMetadata,
    pub totals: UsageTotals,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobInfo {
    pub id: String,
    pub owner_session_id: String,
    pub target_session_id: Option<String>,
    pub run_id: String,
    pub background: bool,
    pub status: JobStatus,
    pub started_at: u64,
    pub finished_at: Option<u64>,
    pub stop_reason: Option<StopReason>,
    pub result: Option<JobResult>,
}
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct QueuedInput {
    pub id: String,
    pub owner: String,
    pub text: String,
    pub follow_up: bool,
}
struct InputEntry {
    input: QueuedInput,
    journal: Option<Arc<SessionJournal>>,
}
struct Entry {
    state: watch::Sender<JobInfo>,
    cancel: Cancel,
    abort: Option<tokio::task::AbortHandle>,
    child: Option<Arc<Agent>>,
    child_runtime: Option<crate::SessionRuntime>,
    totals: Arc<Mutex<UsageTotals>>,
    journal: Option<Arc<SessionJournal>>,
    delivered: bool,
}
struct Inner {
    entries: Mutex<HashMap<String, Entry>>,
    changed: watch::Sender<u64>,
    progress: tokio::sync::broadcast::Sender<(String, AgentEvent)>,
    slots: Arc<tokio::sync::Semaphore>,
    inputs: Mutex<Vec<InputEntry>>,
}
#[derive(Clone)]
pub struct Jobs(Arc<Inner>);
impl Default for Jobs {
    fn default() -> Self {
        Self::with_max_parallel(crate::DEFAULT_MAX_PARALLEL)
    }
}
impl Jobs {
    pub fn with_max_parallel(max_parallel: usize) -> Self {
        Self(Arc::new(Inner {
            entries: Mutex::new(HashMap::new()),
            inputs: Mutex::new(Vec::new()),
            changed: watch::channel(0).0,
            progress: tokio::sync::broadcast::channel(1024).0,
            slots: Arc::new(tokio::sync::Semaphore::new(max_parallel.max(1))),
        }))
    }
}
impl Jobs {
    pub fn list(&self, owner: &str) -> Vec<JobInfo> {
        let mut jobs: Vec<_> = self
            .0
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .map(|entry| entry.state.borrow().clone())
            .filter(|info| info.owner_session_id == owner)
            .collect();
        jobs.sort_by(|a, b| a.started_at.cmp(&b.started_at).then(a.id.cmp(&b.id)));
        jobs
    }
    pub fn get(&self, id: &str) -> Option<JobInfo> {
        self.0
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id)
            .map(|entry| entry.state.borrow().clone())
    }
    pub fn cancel(&self, owner: &str, id: &str) -> bool {
        let entries = self.0.entries.lock().unwrap_or_else(|p| p.into_inner());
        let Some(entry) = entries.get(id) else {
            return false;
        };
        if entry.state.borrow().owner_session_id != owner
            || entry.state.borrow().status != JobStatus::Running
        {
            return false;
        }
        entry.cancel.cancel();
        true
    }
    pub async fn wait(&self, id: &str) -> Result<JobInfo, String> {
        let mut state = self
            .0
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id)
            .ok_or("unknown job")?
            .state
            .subscribe();
        loop {
            let info = state.borrow_and_update().clone();
            if info.status != JobStatus::Running {
                return Ok(info);
            }
            state
                .changed()
                .await
                .map_err(|_| "job stopped without a result")?;
        }
    }
    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.0.changed.subscribe()
    }
    pub(crate) fn progress(&self) -> tokio::sync::broadcast::Receiver<(String, AgentEvent)> {
        self.0.progress.subscribe()
    }
    // Limit active effects, never sessions waiting for their descendants.
    pub(crate) async fn execution_slot(
        &self,
        cancel: &Cancel,
    ) -> Result<tokio::sync::OwnedSemaphorePermit, &'static str> {
        tokio::select! {
            permit = self.0.slots.clone().acquire_owned() => permit.map_err(|_| "runtime is shutting down"),
            _ = crate::execution::wait_for_cancel(cancel) => Err("execution was cancelled while queued"),
        }
    }
    fn notify(&self) {
        self.0
            .changed
            .send_modify(|value| *value = value.wrapping_add(1));
    }
    pub(crate) fn pending(&self, owner: &str) -> Vec<JobInfo> {
        self.0
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .filter(|entry| !entry.delivered)
            .map(|entry| entry.state.borrow().clone())
            .filter(|info| {
                info.background
                    && info.owner_session_id == owner
                    && info.status != JobStatus::Running
            })
            .collect()
    }
    pub(crate) fn acknowledge(&self, id: &str) -> Result<(), StoreError> {
        let mut entries = self.0.entries.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(entry) = entries.get_mut(id) {
            if let Some(journal) = &entry.journal {
                journal.store().acknowledge_job(id)?;
            }
            entry.delivered = true;
        }
        Ok(())
    }
    pub(crate) fn restore(&self, journal: &Arc<SessionJournal>) -> Result<(), StoreError> {
        let mut entries = self.0.entries.lock().unwrap_or_else(|p| p.into_inner());
        for (mut info, delivered) in journal.store().load_jobs(journal.id())? {
            if entries.contains_key(&info.id) {
                continue;
            }
            if info.status == JobStatus::Running {
                info.status = JobStatus::Interrupted;
                info.stop_reason = Some(StopReason::Shutdown);
                info.finished_at = Some(now());
                journal.store().save_job(&info, info.background)?;
            }
            entries.insert(
                info.id.clone(),
                Entry {
                    state: watch::channel(info).0,
                    cancel: Cancel::new(),
                    abort: None,
                    child: None,
                    child_runtime: None,
                    totals: Default::default(),
                    journal: Some(journal.clone()),
                    delivered,
                },
            );
        }
        drop(entries);
        let mut inputs = self.0.inputs.lock().unwrap_or_else(|p| p.into_inner());
        for input in journal.store().load_inputs(journal.id())? {
            if !inputs.iter().any(|entry| entry.input.id == input.id) {
                inputs.push(InputEntry {
                    input,
                    journal: Some(journal.clone()),
                });
            }
        }
        drop(inputs);
        self.notify();
        Ok(())
    }
    pub(crate) fn child(&self, id: &str) -> Option<Arc<Agent>> {
        self.0
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id)
            .and_then(|entry| entry.child.clone())
    }
    pub(crate) fn child_runtime(&self, id: &str) -> Option<crate::SessionRuntime> {
        self.0
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id)
            .and_then(|entry| entry.child_runtime.clone())
    }
    pub(crate) fn totals(&self, id: &str) -> Arc<Mutex<UsageTotals>> {
        self.0
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id)
            .map(|entry| entry.totals.clone())
            .unwrap_or_default()
    }
    pub(crate) fn enqueue(
        &self,
        owner: &str,
        text: &str,
        follow_up: bool,
        journal: Option<Arc<SessionJournal>>,
    ) -> Result<(), StoreError> {
        if text.trim().is_empty() {
            return Ok(());
        }
        let input = QueuedInput {
            id: crate::identity::new_id(),
            owner: owner.into(),
            text: text.into(),
            follow_up,
        };
        let mut inputs = self.0.inputs.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(journal) = &journal {
            journal.store().save_input(&input)?;
        }
        inputs.push(InputEntry { input, journal });
        drop(inputs);
        self.notify();
        Ok(())
    }
    pub(crate) fn pending_inputs(&self, owner: &str, include_followups: bool) -> Vec<QueuedInput> {
        self.0
            .inputs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|entry| {
                entry.input.owner == owner && (include_followups || !entry.input.follow_up)
            })
            .map(|entry| entry.input.clone())
            .collect()
    }
    pub(crate) fn acknowledge_input(&self, id: &str) -> Result<(), StoreError> {
        let mut inputs = self.0.inputs.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(at) = inputs.iter().position(|entry| entry.input.id == id) {
            if let Some(journal) = &inputs[at].journal {
                journal.store().acknowledge_input(id)?;
            }
            inputs.remove(at);
        }
        Ok(())
    }
    pub(crate) fn register_child(&self, id: &str, child: &Arc<Agent>) -> Result<(), StoreError> {
        let runtime = self
            .child_runtime(id)
            .unwrap_or_else(|| crate::SessionRuntime::for_job(child.clone()));
        let mut entries = self.0.entries.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(entry) = entries.get_mut(id) {
            entry.child = Some(child.clone());
            entry.child_runtime = Some(runtime);
            let mut info = entry.state.borrow().clone();
            info.target_session_id = Some(child.session_id());
            if let Some(journal) = &entry.journal {
                journal.store().save_job(&info, false)?;
            }
            entry.state.send_replace(info);
        }
        Ok(())
    }
    pub(crate) fn continue_running(
        &self,
        owner: &str,
        session: &str,
        prompt: &str,
    ) -> Result<Option<String>, String> {
        let entries = self.0.entries.lock().unwrap_or_else(|p| p.into_inner());
        for entry in entries.values() {
            let info = entry.state.borrow();
            if info.owner_session_id == owner
                && (info.target_session_id.as_deref() == Some(session) || info.id == session)
                && info.status == JobStatus::Running
            {
                let child = entry.child.as_ref().ok_or("child is stopping")?;
                self.enqueue(&child.session_id(), prompt, false, child.journal())
                    .map_err(|error| error.to_string())?;
                return Ok(Some(info.id.clone()));
            }
        }
        Ok(None)
    }
    pub(crate) fn start(
        &self,
        runner: Arc<SubagentRunner>,
        task: Task,
        mut ctx: ToolContext,
        tx: UnboundedSender<AgentEvent>,
        call_id: &str,
    ) -> Result<String, StoreError> {
        let id = format!("job-{}", crate::identity::new_id());
        let background = task.background;
        if background {
            ctx.cancel = Cancel::new();
        } else {
            ctx.cancel = ctx.cancel.child();
        }
        let info = JobInfo {
            id: id.clone(),
            run_id: crate::identity::new_id(),
            owner_session_id: runner.owner.clone(),
            target_session_id: task.resume_id.clone(),
            background,
            status: JobStatus::Running,
            started_at: now(),
            finished_at: None,
            stop_reason: None,
            result: None,
        };
        if let Some(journal) = &runner.parent {
            journal.store().save_job(&info, false)?;
        }
        self.0
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(
                id.clone(),
                Entry {
                    state: watch::channel(info).0,
                    cancel: ctx.cancel.clone(),
                    abort: None,
                    child: None,
                    child_runtime: None,
                    totals: Default::default(),
                    journal: runner.parent.clone(),
                    delivered: !background,
                },
            );
        let _ = tx.send(AgentEvent::SubagentStarted {
            call_id: call_id.to_owned(),
            task_id: id.clone(),
        });
        let jobs = self.clone();
        let job_id = id.clone();
        let owner = runner.owner.clone();
        let worker = tokio::spawn(async move {
            // The job owns its event channel; ending a parent's stream cannot cancel a background child.
            let (worker_tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let mut task = task;
            loop {
                let task_input = task.clone();
                let future = async { runner.run(task_input, &ctx, &worker_tx, &job_id).await };
                let future = std::panic::AssertUnwindSafe(future).catch_unwind();
                tokio::pin!(future);
                let outcome = loop {
                    tokio::select! {
                        biased;
                        event=rx.recv()=>{if let Some(event)=event {
                            if background { let _=jobs.0.progress.send((owner.clone(),event)); }
                            else { let _=tx.send(event); }
                        }},
                        result=&mut future=> {
                            while let Ok(event)=rx.try_recv() {
                                if background { let _=jobs.0.progress.send((owner.clone(),event)); }
                                else { let _=tx.send(event); }
                            }
                            break result.unwrap_or_else(|_|ToolOutput::error("subagent execution panicked").into());
                        }
                    }
                };
                let result = JobResult {
                    output: outcome.output.text,
                    is_error: outcome.output.is_error,
                    metadata: *outcome.output.metadata,
                    totals: outcome.totals,
                };
                if jobs.finish(&job_id, result, ctx.is_cancelled()) {
                    break;
                }
                task.prompt = "Continue with the queued instructions.".into();
            }
            drop(worker_tx);
            jobs.notify();
        });
        if let Some(entry) = self
            .0
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get_mut(&id)
        {
            entry.abort = Some(worker.abort_handle());
        }
        self.notify();
        Ok(id)
    }
    fn finish(&self, id: &str, result: JobResult, cancelled: bool) -> bool {
        let mut entries = self.0.entries.lock().unwrap_or_else(|p| p.into_inner());
        let Some(entry) = entries.get_mut(id) else {
            return true;
        };
        let mut info = entry.state.borrow().clone();
        if info.status != JobStatus::Running {
            return true;
        }
        if !cancelled
            && !result.is_error
            && let Some(session) = &info.target_session_id
            && !self.pending_inputs(session, true).is_empty()
        {
            return false;
        }
        let partial = result.metadata.details["finish_reason"] == "step_limit";
        info.status = if cancelled {
            JobStatus::Cancelled
        } else if result.is_error {
            JobStatus::Failed
        } else if partial {
            JobStatus::Partial
        } else {
            JobStatus::Completed
        };
        info.stop_reason = Some(if cancelled {
            StopReason::UserCancelled
        } else if result.is_error {
            StopReason::ExecutionError
        } else if partial {
            StopReason::StepLimit
        } else {
            StopReason::Completed
        });
        info.finished_at = Some(now());
        info.result = Some(result);
        if let Some(journal) = &entry.journal
            && let Err(error) = journal.store().save_job(&info, info.background)
        {
            info.status = JobStatus::Failed;
            info.stop_reason = Some(StopReason::ExecutionError);
            if let Some(result) = &mut info.result {
                result.is_error = true;
                result.output = format!("cannot persist job completion: {error}");
            }
        }
        entry.child = None;
        entry.child_runtime = None;
        entry.abort = None;
        entry.state.send_replace(info);
        drop(entries);
        self.notify();
        true
    }
    /// Stop owned executions on application shutdown; never replay their tools on recovery.
    pub fn shutdown(&self) {
        let mut entries = self.0.entries.lock().unwrap_or_else(|p| p.into_inner());
        for entry in entries.values_mut() {
            let mut info = entry.state.borrow().clone();
            if info.status != JobStatus::Running {
                continue;
            }
            entry.cancel.cancel();
            info.status = JobStatus::Interrupted;
            info.stop_reason = Some(StopReason::Shutdown);
            info.finished_at = Some(now());
            if let Some(journal) = &entry.journal {
                let _ = journal.store().save_job(&info, info.background);
            }
            entry.state.send_replace(info);
            entry.child = None;
            entry.child_runtime = None;
            if let Some(abort) = entry.abort.take() {
                abort.abort();
            }
        }
        drop(entries);
        self.notify();
    }
}
