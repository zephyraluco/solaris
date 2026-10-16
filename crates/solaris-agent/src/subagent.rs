//! Bounded, foreground delegation with isolated conversations and inherited policy.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use solaris_backend::{
    AgentBackend, AgentEvent, AgentEventStream, BackendError, TurnRequest, Usage,
};
use solaris_provider::BackendChoice;
use solaris_tools::{Cancel, Tool, ToolContext, ToolOutput, ToolRegistry, ToolSelection};
use tokio::sync::mpsc::UnboundedSender;

use crate::{Agent, AgentOptions, Mode, ResourceSnapshot, default_tool_names};

/// Runtime tool for delegating a self-contained task to an isolated child agent.
pub const SUBAGENT_TOOL: &str = "subagent";
const MAX_CALLS: usize = 20;
const TIMEOUT: Duration = Duration::from_secs(300);
const MAX_RESULT_BYTES: usize = 16 * 1024;

/// Built-in file/shell tools plus the agent runtime's delegation declaration.
/// Used by CLI tool-selection validation as well as sessions.
pub fn agent_tool_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::builtin();
    registry.add(Arc::new(SubagentTool));
    registry
}

/// The declaration lives above solaris-tools: only the agent can run a child.
pub(crate) struct SubagentTool;

#[async_trait]
impl Tool for SubagentTool {
    fn name(&self) -> &str {
        SUBAGENT_TOOL
    }

    fn description(&self) -> &str {
        "Delegate a self-contained task to an isolated subagent using the current model: explore (read-only) or general (Build mode only). Waits for its result."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "agent": { "type": "string", "enum": ["explore", "general"] },
                "prompt": { "type": "string", "description": "Complete task, relevant context, paths, constraints and expected result. The child cannot see the parent conversation." }
            },
            "required": ["agent", "prompt"],
            "additionalProperties": false
        })
    }

    fn guideline(&self) -> Option<&'static str> {
        Some(
            "Use explore for focused read-only codebase research, general for a bounded implementation task in Build mode. Supply all relevant context; children see project instructions but not this conversation. Independent tasks may run in parallel; give writing tasks disjoint files. Children share the workspace, not a filesystem sandbox, and cannot delegate further. Review their results before relying on them. Each call waits for completion and is limited to 20 model requests / 5 minutes; return only a concise final report.",
        )
    }

    async fn run(&self, _input: Value, _ctx: &ToolContext) -> ToolOutput {
        ToolOutput::error("subagent requires the agent runtime")
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Explore,
    General,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Task {
    agent: Kind,
    prompt: String,
}

/// Snapshot captured at the parent's turn boundary, not a reference to its history.
pub(crate) struct SubagentRunner {
    pub choice: BackendChoice,
    pub registry: Arc<ToolRegistry>,
    pub options: AgentOptions,
    pub resources: ResourceSnapshot,
    pub mode: Mode,
}

#[derive(Default, Clone, Copy)]
pub(crate) struct Totals {
    pub usage: Usage,
    pub cost_usd: f64,
}

pub(crate) struct SubagentResult {
    pub output: ToolOutput,
    pub totals: Totals,
}

impl From<ToolOutput> for SubagentResult {
    fn from(output: ToolOutput) -> Self {
        Self {
            output,
            totals: Totals::default(),
        }
    }
}

impl SubagentRunner {
    pub async fn run(
        &self,
        input: Value,
        ctx: &ToolContext,
        tx: &UnboundedSender<AgentEvent>,
        call_id: &str,
    ) -> SubagentResult {
        let task: Task = match serde_json::from_value(input) {
            Ok(task) => task,
            Err(_) => return ToolOutput::error("subagent requires an agent (explore or general) and a prompt string, with no other fields").into(),
        };
        if task.prompt.trim().is_empty() || task.prompt.len() > 64 * 1024 {
            return ToolOutput::error("subagent prompt must be non-empty and at most 64 KiB")
                .into();
        }
        if ctx.is_cancelled() {
            return ToolOutput::error("subagent was cancelled before it started").into();
        }
        if self.mode == Mode::Plan && matches!(task.agent, Kind::General) {
            return ToolOutput::error(
                "Plan mode only permits explore subagents; general would allow changes",
            )
            .into();
        }

        let mode = match task.agent {
            Kind::Explore => Mode::Plan,
            Kind::General => self.mode,
        };
        // An explicit allowlist/exclusion applies to the child too. Explore can
        // search/list even when Build's default declaration prefers the shell.
        let tools = self
            .registry
            .select(default_tool_names(mode), &self.options.selection)
            .into_iter()
            .filter(|tool| tool.name() != SUBAGENT_TOOL)
            .filter(|tool| {
                mode != Mode::Plan || matches!(tool.name(), "read" | "grep" | "find" | "ls")
            })
            .collect();
        let registry = Arc::new(ToolRegistry::with_tools(tools));
        let totals = Arc::new(Mutex::new(Totals::default()));
        let backend = Arc::new(BoundedBackend {
            inner: Arc::clone(&self.choice.backend),
            requests: AtomicUsize::new(0),
            totals: Arc::clone(&totals),
        });
        let mut choice = self.choice.clone();
        choice.backend = backend;
        let mut options = self.options.clone().with_subagents(false);
        options.cwd = ctx.cwd.clone();
        options.temp_dir = ctx.temp_dir.clone();
        // Preserve explicit additions/custom tools instead of reapplying the
        // mode's defaults to the already-filtered child registry.
        options.selection = ToolSelection {
            only: registry.names(),
            ..Default::default()
        };
        options.skills = self.resources.skills.clone();
        options.prompt_context = self.resources.prompt.clone();
        let child = Agent::new(choice, registry, options);
        let cancel = child.cancel_handle();
        let mut stream = child.run_turn(format!(
            "You are an isolated {:?} subagent. Complete only the task below and return a concise final report with findings, changed files, verification and limitations. You cannot delegate.\n\n{}",
            task.agent, task.prompt), mode);
        // A dropped parent future must also stop its child, including when the
        // child is waiting on network IO or on an approval.
        let _cancel_on_drop = CancelOnDrop(cancel.clone());
        let label = format!("subagent {call_id} ({:?})", task.agent);
        let _ = tx.send(AgentEvent::Status(format!("{label}: running")));
        let output = collect(&mut stream, ctx, tx, &label).await;
        let _ = tx.send(AgentEvent::Status(format!(
            "{label}: {}",
            if output.is_error {
                "failed"
            } else {
                "complete"
            }
        )));
        let totals = *totals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        SubagentResult { output, totals }
    }
}

struct CancelOnDrop(Cancel);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

async fn collect(
    stream: &mut AgentEventStream,
    ctx: &ToolContext,
    tx: &UnboundedSender<AgentEvent>,
    label: &str,
) -> ToolOutput {
    let deadline = tokio::time::sleep(TIMEOUT);
    tokio::pin!(deadline);
    let mut ticker = tokio::time::interval(Duration::from_millis(50));
    let mut text = String::new();
    let mut clipped = false;
    let mut completed = false;
    loop {
        tokio::select! {
            biased;
            _ = ticker.tick() => {
                if ctx.is_cancelled() || tx.is_closed() { return ToolOutput::error("subagent was cancelled"); }
            }
            _ = &mut deadline => return ToolOutput::error("subagent timed out after 5 minutes"),
            event = stream.next() => match event {
                Some(AgentEvent::TextDelta(chunk)) => {
                    let remaining = MAX_RESULT_BYTES.saturating_sub(text.len());
                    let mut end = remaining.min(chunk.len());
                    while !chunk.is_char_boundary(end) { end -= 1; }
                    text.push_str(&chunk[..end]);
                    clipped |= end < chunk.len();
                }
                Some(AgentEvent::ToolCall(call)) => {
                    // Earlier round text is progress, not the child's final answer.
                    text.clear();
                    clipped = false;
                    let _ = tx.send(AgentEvent::Status(format!("{label}: {}", call.name)));
                }
                Some(AgentEvent::Error(error)) => return ToolOutput::error(format!("{label} failed: {error}")),
                Some(AgentEvent::TurnComplete { .. }) => completed = true,
                None => break,
                _ => {}
            }
        }
    }
    if !completed {
        return ToolOutput::error("subagent ended without completing");
    }
    if text.trim().is_empty() {
        return ToolOutput::error("subagent completed without a final report");
    }
    if clipped {
        text.push_str("\n[subagent result truncated to 16 KiB]");
    }
    ToolOutput::ok(text)
}

/// Counts every provider request (including compaction), even when the child
/// eventually fails. Never reports a child's usage as the parent's context size.
struct BoundedBackend {
    inner: Arc<dyn AgentBackend>,
    requests: AtomicUsize,
    totals: Arc<Mutex<Totals>>,
}

#[async_trait]
impl AgentBackend for BoundedBackend {
    async fn run_turn(&self, request: TurnRequest) -> Result<AgentEventStream, BackendError> {
        if self.requests.fetch_add(1, Ordering::Relaxed) >= MAX_CALLS {
            return Err(BackendError::new(
                "subagent exceeded its 20 model-request budget",
            ));
        }
        let stream = self.inner.run_turn(request).await?;
        let totals = Arc::clone(&self.totals);
        Ok(Box::pin(stream.map(move |event| {
            if let AgentEvent::TurnComplete { usage, cost_usd } = &event {
                let mut totals = totals
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                totals.usage = totals.usage.merge(*usage);
                totals.cost_usd += cost_usd;
            }
            event
        })))
    }
    fn label(&self) -> &str {
        self.inner.label()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_child_that_never_answers_times_out() {
        let mut stream: AgentEventStream = Box::pin(futures::stream::pending());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let output = collect(&mut stream, &ToolContext::new(".", "."), &tx, "child").await;
        assert!(output.is_error);
        assert!(output.text.contains("timed out"));
    }

    #[tokio::test]
    async fn returned_text_is_bounded_on_utf8_boundaries_and_marks_truncation() {
        let mut stream: AgentEventStream = Box::pin(futures::stream::iter(vec![
            AgentEvent::TextDelta("中文".repeat(MAX_RESULT_BYTES)),
            AgentEvent::TurnComplete {
                usage: Usage::new(1, 1),
                cost_usd: 0.0,
            },
        ]));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let output = collect(&mut stream, &ToolContext::new(".", "."), &tx, "child").await;
        assert!(!output.is_error);
        assert!(output.text.len() < MAX_RESULT_BYTES + 100);
        assert!(output.text.contains("truncated"));
    }

    #[tokio::test]
    async fn a_child_stream_without_completion_is_not_reported_as_success() {
        let mut stream: AgentEventStream =
            Box::pin(futures::stream::iter(vec![AgentEvent::TextDelta(
                "partial".into(),
            )]));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let output = collect(&mut stream, &ToolContext::new(".", "."), &tx, "child").await;
        assert!(output.is_error);
        assert!(output.text.contains("without completing"));
    }

    #[test]
    fn tool_schema_and_parser_reject_unknown_modes_fields_and_non_string_prompts() {
        for input in [
            json!({"agent":"unknown","prompt":"task"}),
            json!({"agent":"explore","prompt":1}),
            json!({"agent":"explore","prompt":"task","model":"other"}),
            json!({"prompt":"task"}),
        ] {
            assert!(serde_json::from_value::<Task>(input).is_err());
        }
        assert_eq!(SubagentTool.parameters()["additionalProperties"], false);
    }
}
