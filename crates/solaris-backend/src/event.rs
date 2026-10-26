//! Events streamed from a backend during a turn.

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
    /// Usage spent in isolated subagent contexts. Included in the eventual
    /// turn total, but must not be counted as occupying the parent's window.
    SubagentUsage { usage: Usage, cost_usd: f64 },
    /// Associates a running child with the parent tool call that launched it.
    /// Display-only: these events never become parent conversation messages.
    SubagentStarted { call_id: String, task_id: String },
    /// A tool requested inside one child, not a top-level parent tool call.
    SubagentToolCall { task_id: String, call: ToolCall },
    /// The matching child tool result, kept under that child's transcript row.
    SubagentToolResult {
        task_id: String,
        result: ToolResult,
        duration_ms: u64,
    },
    /// A background child finished. Its launch already has a tool result, so
    /// this notification is not a second wire-level result for the same call.
    /// The runtime feeds it to the model between rounds, before ending the turn.
    SubagentComplete {
        task_id: String,
        output: String,
        is_error: bool,
        duration_ms: u64,
    },
    /// A message the user queued for after this turn is now being run as one.
    ///
    /// Not terminal: the turn it arrived during is over, and another has begun in
    /// the same loop. It is reported so the caller can attribute what follows to
    /// the right prompt rather than appending it to the turn that just finished.
    FollowUp {
        /// The prompt of the turn that is starting.
        prompt: String,
    },
    /// The model hit its output limit, so the message it produced is cut off.
    ///
    /// Not terminal: the message still arrives in full as far as the provider
    /// sent it, and it may carry tool calls whose arguments were cut mid-JSON.
    /// A reader that runs calls has to refuse them rather than act on a
    /// half-written request, which is why the signal is its own event rather
    /// than a detail of the completion.
    OutputTruncated,
    /// Earlier messages were replaced by a summary of them.
    ///
    /// Not terminal, and not an error: the turn continues with less to read.
    /// It is its own event because the caller is the only one that can say so —
    /// a turn that suddenly remembers less is worth explaining to the user
    /// rather than leaving as a mystery.
    Compacted {
        /// How many messages the summary replaced.
        dropped: usize,
    },
    /// A message the user steered in has been folded into the conversation.
    ///
    /// Not terminal. The loop reports it rather than the caller assuming it,
    /// because the loop is what decides *when* the message is read: one steered
    /// in as a turn ends is read by the next turn instead, and a caller that had
    /// written it down on the turn it was typed into would then have it in the
    /// record twice.
    Steered {
        /// What the user typed.
        text: String,
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
///
/// The mode a turn runs in is deliberately absent: which tools a turn may use is
/// the agent's decision, and by the time a request reaches a backend its tool
/// list is already settled.
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
    /// Tools declared to the model for this request. Empty means none, and the
    /// request bodies then carry no `tools` field at all.
    pub tools: Vec<ToolSpec>,
}

impl TurnRequest {
    /// A request that declares no tools.
    pub fn new(history: Vec<Message>, prompt: impl Into<String>) -> Self {
        Self {
            history,
            prompt: prompt.into(),
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

        assert!(!AgentEvent::ToolCall(call.clone()).is_terminal());
        assert!(
            !AgentEvent::ToolResult {
                result,
                duration_ms: 3
            }
            .is_terminal()
        );
        assert!(!AgentEvent::TextDelta("hi".to_string()).is_terminal());
        for event in [
            AgentEvent::SubagentStarted {
                call_id: "launch".into(),
                task_id: "child".into(),
            },
            AgentEvent::SubagentToolCall {
                task_id: "child".into(),
                call: call.clone(),
            },
            AgentEvent::SubagentToolResult {
                task_id: "child".into(),
                result: ToolResult::ok(&call, "data"),
                duration_ms: 1,
            },
        ] {
            assert!(!event.is_terminal());
        }
        assert!(
            !AgentEvent::SubagentUsage {
                usage: Usage::new(1, 1),
                cost_usd: 0.0
            }
            .is_terminal()
        );
        assert!(
            !AgentEvent::SubagentComplete {
                task_id: "subagent-1".into(),
                output: "report".into(),
                is_error: false,
                duration_ms: 1,
            }
            .is_terminal()
        );
        assert!(
            !AgentEvent::OutputTruncated.is_terminal(),
            "a cut-off message still has a completion after it"
        );
        assert!(
            !AgentEvent::Compacted { dropped: 4 }.is_terminal(),
            "a compacted conversation still has a completion after it"
        );
        assert!(
            !AgentEvent::Steered {
                text: "use the other file".to_string()
            }
            .is_terminal(),
            "a steered message still has a completion after it"
        );
        assert!(
            !AgentEvent::FollowUp {
                prompt: "then run the tests".to_string()
            }
            .is_terminal(),
            "a follow-up leaves the turn it interrupted finished"
        );
        assert!(AgentEvent::Error("boom".to_string()).is_terminal());
    }

    #[test]
    fn a_new_request_declares_no_tools() {
        let request = TurnRequest::new(Vec::new(), "hi");
        assert!(request.tools.is_empty());
        assert_eq!(request.prompt, "hi");
    }
}
