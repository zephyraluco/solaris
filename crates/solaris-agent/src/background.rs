//! Turn-scoped child tasks: launching never waits, dropping aborts every child.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::time::Instant;

use futures::future::poll_fn;
use serde_json::json;
use solaris_backend::AgentEvent;
use solaris_tools::{ToolContext, ToolOutput};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use crate::subagent::{SubagentResult, SubagentRunner, Task};

#[derive(Default)]
pub(crate) struct BackgroundTasks {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    next_id: u64,
    tasks: Vec<BackgroundTask>,
}

struct BackgroundTask {
    id: String,
    started: Instant,
    tx: UnboundedSender<AgentEvent>,
    handle: JoinHandle<Completion>,
}

pub(crate) struct Completion {
    pub task_id: String,
    pub result: SubagentResult,
    duration_ms: u64,
}

impl Completion {
    fn notify(&self, tx: &UnboundedSender<AgentEvent>) {
        let _ = tx.send(AgentEvent::SubagentComplete {
            task_id: self.task_id.clone(),
            output: self.result.output.text.clone(),
            is_error: self.result.output.is_error,
            duration_ms: self.duration_ms,
        });
    }
}

impl BackgroundTasks {
    pub fn start(
        &self,
        runner: Arc<SubagentRunner>,
        task: Task,
        ctx: ToolContext,
        tx: UnboundedSender<AgentEvent>,
    ) -> ToolOutput {
        let mut state = self.lock();
        state.next_id += 1;
        let id = format!("subagent-{}", state.next_id);
        let task_id = id.clone();
        let started = Instant::now();
        let worker_tx = tx.clone();
        let handle = tokio::spawn(async move {
            let result = runner.run(task, &ctx, &worker_tx, &task_id).await;
            let completion = Completion {
                task_id,
                result,
                duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            };
            // Notify the UI even while the parent is streaming or running a
            // tool. Only the main loop mutates history, at a safe round boundary.
            completion.notify(&worker_tx);
            completion
        });
        let output = ToolOutput::ok(json!({"task_id": id, "status": "started"}).to_string());
        state.tasks.push(BackgroundTask {
            id,
            started,
            tx,
            handle,
        });
        output
    }

    pub fn is_empty(&self) -> bool {
        self.lock().tasks.is_empty()
    }

    pub fn try_next(&self) -> Option<Completion> {
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        match self.poll_next(&mut cx) {
            Poll::Ready(completion) => completion,
            Poll::Pending => None,
        }
    }

    pub async fn next(&self) -> Option<Completion> {
        poll_fn(|cx| self.poll_next(cx)).await
    }

    fn poll_next(&self, cx: &mut Context<'_>) -> Poll<Option<Completion>> {
        let mut state = self.lock();
        for index in 0..state.tasks.len() {
            if let Poll::Ready(result) = Pin::new(&mut state.tasks[index].handle).poll(cx) {
                let task = state.tasks.swap_remove(index);
                let completion = result.unwrap_or_else(|error| {
                    let completion = Completion {
                        task_id: task.id,
                        result: ToolOutput::error(format!("background subagent failed: {error}"))
                            .into(),
                        duration_ms: u64::try_from(task.started.elapsed().as_millis())
                            .unwrap_or(u64::MAX),
                    };
                    completion.notify(&task.tx);
                    completion
                });
                return Poll::Ready(Some(completion));
            }
        }
        if state.tasks.is_empty() {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Drop for BackgroundTasks {
    fn drop(&mut self) {
        // Aborting drops SubagentRunner::run, whose child stream and cancellation
        // guard also stop pending network requests, tools and approval waits.
        for task in &self.lock().tasks {
            task.handle.abort();
        }
    }
}
