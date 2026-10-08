//! Events streamed from a backend during a turn.

use crate::config::Mode;
use crate::message::Message;
use crate::tool::{ToolCall, ToolResult, ToolSpec};
use crate::usage::Usage;
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
    /// The model asked for a tool to run.
    ///
    /// Not terminal: the answer follows as [`AgentEvent::ToolResult`], and the
    /// turn only ends when the model stops asking.
    ToolCall(ToolCall),
    /// What running a requested tool produced, and how long it took.
    ToolResult {
        result: ToolResult,
        duration_ms: u64,
    },
    /// Terminal event: the turn finished successfully.
    TurnComplete {
        /// What the turn consumed, measured by the provider or estimated when
        /// it reported nothing.
        usage: Usage,
        /// What that usage costs at the model's list price, in USD.
        cost_usd: f64,
    },
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
    ///
    /// Empty means the caller has already put the prompt in `history` — which
    /// is what a tool round trip does — and the request bodies then append no
    /// further user message.
    pub prompt: String,
    /// Active agent mode.
    pub mode: Mode,
    /// Tools declared to the model for this request. Empty means none, and the
    /// request bodies then carry no `tools` field at all.
    pub tools: Vec<ToolSpec>,
}

impl TurnRequest {
    /// A request with no tools declared.
    pub fn new(history: Vec<Message>, prompt: impl Into<String>, mode: Mode) -> Self {
        Self {
            history,
            prompt: prompt.into(),
            mode,
            tools: Vec::new(),
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tool_exchange_does_not_end_the_turn() {
        let call = ToolCall::new("call-1", "read", serde_json::json!({}));
        let result = ToolResult::ok(&call, "ok");

        assert!(!AgentEvent::ToolCall(call).is_terminal());
        assert!(
            !AgentEvent::ToolResult {
                result,
                duration_ms: 3
            }
            .is_terminal()
        );
        assert!(!AgentEvent::TextDelta("hi".to_string()).is_terminal());
        assert!(AgentEvent::Error("boom".to_string()).is_terminal());
    }

    #[test]
    fn a_new_request_declares_no_tools() {
        let request = TurnRequest::new(Vec::new(), "hi", Mode::Build);
        assert!(request.tools.is_empty());
        assert_eq!(request.prompt, "hi");
    }
}
