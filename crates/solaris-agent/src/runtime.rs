//! Persistent event boundary shared by interactive and headless clients.

use crate::{AgentEvent, AgentEventStream, SessionJournal, SessionRecord};
use futures::StreamExt;
use std::sync::Arc;

pub(crate) fn journal_stream(
    stream: AgentEventStream,
    journal: Option<Arc<SessionJournal>>,
) -> AgentEventStream {
    let Some(journal) = journal else {
        return stream;
    };
    Box::pin(futures::stream::unfold(
        (stream, journal, false),
        |(mut stream, journal, failed)| async move {
            if failed {
                return None;
            }
            let event = stream.next().await?;
            match journal.append(&SessionRecord::Event {
                event: event.clone(),
            }) {
                Ok(()) => Some((event, (stream, journal, false))),
                Err(error) => Some((
                    AgentEvent::Error(format!("cannot persist session: {error}")),
                    (stream, journal, true),
                )),
            }
        },
    ))
}

/// A single active turn owns the session until its stream is dropped.
pub(crate) struct TurnLease {
    active: Arc<std::sync::atomic::AtomicBool>,
}

impl TurnLease {
    pub(crate) fn acquire(active: Arc<std::sync::atomic::AtomicBool>) -> Option<Self> {
        active
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .ok()?;
        Some(Self { active })
    }
}

impl Drop for TurnLease {
    fn drop(&mut self) {
        self.active
            .store(false, std::sync::atomic::Ordering::Release);
    }
}

pub(crate) fn leased_stream(stream: AgentEventStream, lease: TurnLease) -> AgentEventStream {
    Box::pin(LeasedStream {
        stream: Some(stream),
        _lease: lease,
    })
}

struct LeasedStream {
    stream: Option<AgentEventStream>,
    _lease: TurnLease,
}
impl futures::Stream for LeasedStream {
    type Item = AgentEvent;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        match self.stream.as_mut() {
            Some(stream) => stream.as_mut().poll_next(cx),
            None => std::task::Poll::Ready(None),
        }
    }
}
impl Drop for LeasedStream {
    fn drop(&mut self) {
        drop(self.stream.take());
    }
}

/// Client-independent runtime. UI code owns rendering; this owns session commands.
pub struct SessionRuntime {
    agent: crate::Agent,
}
impl SessionRuntime {
    pub fn new(agent: crate::Agent) -> Self {
        Self { agent }
    }
    pub fn with_refreshed_backend(&self, choice: solaris_provider::BackendChoice) -> Self {
        Self::new(self.agent.with_refreshed_backend(choice))
    }
    pub fn with_child_backend(self, factory: crate::ChildBackendFactory) -> Self {
        Self::new(self.agent.with_child_backend(factory))
    }
    pub fn dispatch(&self, command: RuntimeCommand) -> AgentEventStream {
        match command {
            RuntimeCommand::Prompt { text, mode } => self.agent.run_turn(text, mode),
            RuntimeCommand::Cancel => {
                self.agent.cancel_handle().cancel();
                Box::pin(futures::stream::empty())
            }
            RuntimeCommand::Steer { text } => {
                self.agent.steering_handle().push(text);
                Box::pin(futures::stream::empty())
            }
            RuntimeCommand::Queue { text } => {
                self.agent.follow_up_handle().push(text);
                Box::pin(futures::stream::empty())
            }
        }
    }
}
impl std::ops::Deref for SessionRuntime {
    type Target = crate::Agent;
    fn deref(&self) -> &Self::Target {
        &self.agent
    }
}

/// Commands accepted by both interactive clients and external integrations.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeCommand {
    Prompt { text: String, mode: crate::Mode },
    Cancel,
    Steer { text: String },
    Queue { text: String },
}
