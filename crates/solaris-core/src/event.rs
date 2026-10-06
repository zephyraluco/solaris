//! Events streamed from a backend during a turn.

use crate::config::Mode;
use crate::message::Message;
use thiserror::Error;

/// A single incremental event produced while answering a turn.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    /// Extended-thinking token chunk.
    ThinkingDelta(String),
    /// Assistant visible text chunk.
    TextDelta(String),
    /// Transient status line (e.g. "thinking…", "tool call").
    Status(String),
    /// Terminal event: the turn finished successfully.
    TurnComplete { tokens: u32, cost_usd: f64 },
    /// Terminal event: the turn failed.
    Error(String),
}

impl AgentEvent {
    /// Whether this event ends the turn.
    pub fn is_terminal(&self) -> bool {
        matches!(self, AgentEvent::TurnComplete { .. } | AgentEvent::Error(_))
    }
}

/// Everything a backend needs to answer one turn.
#[derive(Debug, Clone)]
pub struct TurnRequest {
    /// Prior conversation, oldest first, excluding the new prompt.
    pub history: Vec<Message>,
    /// The new user prompt.
    pub prompt: String,
    /// Active agent mode.
    pub mode: Mode,
}

/// Errors a backend can fail with before/while streaming.
#[derive(Debug, Error)]
pub enum BackendError {
    #[error("{0}")]
    Message(String),
}

impl BackendError {
    pub fn new(message: impl Into<String>) -> Self {
        BackendError::Message(message.into())
    }
}
