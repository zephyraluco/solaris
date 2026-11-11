//! Host driver boundary for the official agent's multi-turn state machine.
//!
//! The SDK owns model/tool/done transitions. The host performs effects and keeps
//! the durable conversation, including steering and compaction, authoritative.
use rig_agent::run::{AgentRun, AgentRunStep, ModelTurn, PendingToolCall, TurnPolicy};
use rig_core::message::{AssistantContent, AssistantMessage, ToolName};
use rig_core::tool::{ToolExecutionError, ToolOutput, ToolResult as ModelToolResult};
use solaris_backend::{BackendError, ToolCall, ToolResult};

pub(super) enum Step {
    Model,
    Tools(Vec<ToolCall>),
    Done,
}

pub(super) struct Execution {
    run: AgentRun,
    pending: Vec<PendingToolCall>,
}
impl Execution {
    pub fn new(prompt: &str) -> Self {
        Self {
            // Solaris budgets bound calls, tokens, money and wall time. Do not
            // add the SDK's default single-call ceiling on top of those limits.
            run: AgentRun::new(prompt).max_turns(usize::MAX),
            pending: Vec::new(),
        }
    }

    pub fn next(&mut self) -> Result<Step, BackendError> {
        match self.run.next_step().map_err(error)? {
            AgentRunStep::CallModel { .. } => Ok(Step::Model),
            AgentRunStep::CallTools { calls } => {
                let tools = calls
                    .iter()
                    .map(|call| match call {
                        PendingToolCall::Execute(call) => Ok(ToolCall::new(
                            call.id().wire().into_owned(),
                            call.name().as_str(),
                            call.arguments(),
                        )),
                        PendingToolCall::Malformed(call) => Ok(ToolCall::new(
                            call.id().wire().into_owned(),
                            call.name().as_str(),
                            serde_json::Value::String(call.raw_arguments().into()),
                        )),
                        _ => Err(BackendError::new("unsupported pending tool call")),
                    })
                    .collect::<Result<_, _>>()?;
                self.pending = calls;
                Ok(Step::Tools(tools))
            }
            AgentRunStep::Done(_) => Ok(Step::Done),
        }
    }

    /// Drop the SDK's ephemeral history after host compaction, at an already
    /// opened model boundary. Accounting ceilings remain in the shared budget.
    pub fn rebase(&mut self) -> Result<(), BackendError> {
        if !self.pending.is_empty() {
            return Err(BackendError::new(
                "cannot replace context with unanswered tool calls",
            ));
        }
        let mut fresh = Self::new("compacted session");
        if !matches!(fresh.next()?, Step::Model) {
            return Err(BackendError::new("could not prepare compacted execution"));
        }
        *self = fresh;
        Ok(())
    }

    pub fn accept(&mut self, text: &str, calls: &[ToolCall]) -> Result<(), BackendError> {
        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(AssistantContent::text(text));
        }
        let mut names = std::collections::BTreeSet::new();
        for call in calls {
            let name = ToolName::new(&call.name).map_err(error)?;
            names.insert(call.name.clone());
            content.push(AssistantContent::tool_call(
                &call.id,
                name,
                call.input.clone(),
            ));
        }
        // Availability, JSON schema and approval are enforced by the host's
        // existing tool registry before any side effect. Unknown tools receive
        // ordinary error results, preserving the existing recovery contract.
        let policy = TurnPolicy::new(names, None, None).map_err(error)?;
        let turn = ModelTurn::new(
            AssistantMessage::new(Vec::new()),
            content,
            Default::default(),
            policy,
            serde_json::Value::Null,
        );
        self.run.model_response(turn).map_err(error)?;
        Ok(())
    }

    pub fn answer(&mut self, results: &[ToolResult]) -> Result<(), BackendError> {
        if results.len() != self.pending.len() {
            return Err(BackendError::new(
                "tool result count does not match pending calls",
            ));
        }
        let mut answers = Vec::with_capacity(results.len());
        for (call, result) in self.pending.drain(..).zip(results) {
            match call {
                PendingToolCall::Execute(call) => {
                    if call.id().wire() != result.id || call.name().as_str() != result.name {
                        return Err(BackendError::new("tool result does not match pending call"));
                    }
                    let output = ToolOutput::text(&result.output);
                    let answer = if result.is_error {
                        ModelToolResult::failed(
                            ToolExecutionError::other(&result.output).with_model_output(output),
                        )
                    } else {
                        ModelToolResult::success(output)
                    };
                    answers.push(call.answer(answer));
                }
                PendingToolCall::Malformed(call) => {
                    if call.id().wire() != result.id
                        || call.name().as_str() != result.name
                        || !result.is_error
                    {
                        return Err(BackendError::new(
                            "malformed call requires a matching error result",
                        ));
                    }
                    answers.push(call.answer(None));
                }
                _ => return Err(BackendError::new("unsupported pending tool call")),
            }
        }
        self.run.answer_all(answers).map_err(error)
    }
}
fn error(error: impl std::fmt::Display) -> BackendError {
    BackendError::new(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn tools_must_be_answered_before_the_next_model_call() {
        let mut run = Execution::new("read");
        assert!(matches!(run.next().unwrap(), Step::Model));
        let call = ToolCall::new("1", "read", json!({"path":"a"}));
        run.accept("", std::slice::from_ref(&call)).unwrap();
        assert!(matches!(run.next().unwrap(), Step::Tools(calls) if calls.len() == 1));
        assert!(run.answer(&[]).is_err());
        run.answer(&[ToolResult::ok(&call, "contents")]).unwrap();
        assert!(matches!(run.next().unwrap(), Step::Model));
        run.accept("done", &[]).unwrap();
        assert!(matches!(run.next().unwrap(), Step::Done));
    }
    #[test]
    fn compaction_discards_working_history_after_tools_are_answered() {
        let mut run = Execution::new("read");
        run.next().unwrap();
        let call = ToolCall::new("1", "read", json!({"path":"a"}));
        run.accept("", std::slice::from_ref(&call)).unwrap();
        run.next().unwrap();
        assert!(run.rebase().is_err());
        run.answer(&[ToolResult::ok(&call, "large tool output")])
            .unwrap();
        run.next().unwrap();
        run.rebase().unwrap();
        assert!(
            run.run
                .full_history()
                .iter()
                .all(|message| !format!("{message:?}").contains("large tool output"))
        );
        run.accept("summary-based answer", &[]).unwrap();
        assert!(matches!(run.next().unwrap(), Step::Done));
    }

    #[test]
    fn malformed_arguments_cannot_be_answered_as_success() {
        let mut run = Execution::new("read");
        run.next().unwrap();
        let call = ToolCall::new("1", "read", json!("unfinished"));
        run.accept("", std::slice::from_ref(&call)).unwrap();
        assert!(matches!(run.next().unwrap(), Step::Tools(_)));
        assert!(run.answer(&[ToolResult::ok(&call, "done")]).is_err());
    }
}
