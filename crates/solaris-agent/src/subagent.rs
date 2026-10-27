//! Bounded delegation with isolated conversations and inherited policy.

use std::collections::HashMap;
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
use tokio::sync::{Semaphore, mpsc::UnboundedSender};

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
        "Delegate a self-contained task to an isolated subagent using the current model or an explicit model override: explore (read-only) or general (Build mode only). Waits by default; background=true returns a task ID immediately and notifies you when it finishes."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "agent": { "type": "string", "enum": ["explore", "general"] },
                "name": { "type": "string", "description": "Short task name shown in the subagent summary. Describe the task, not its tools." },
                "prompt": { "type": "string", "description": "Complete task, relevant context, paths, constraints and expected result. The child cannot see the parent conversation." },
                "model": { "type": "string", "description": "Optional child model ID under the current provider. Omit or leave blank to use the parent's current model. Inherits parent policy and budget." },
                "resume_id": { "type": "string", "description": "Resume one of this parent's saved child sessions. Omit or leave blank for a new child. Never automatically reruns interrupted tools." },
                "background": { "type": "boolean", "default": false, "description": "Return a task ID immediately so you can continue independent work. The final report or failure is delivered automatically; do not poll. Background work is scoped to this turn." }
            },
            "required": ["agent", "prompt"],
            "additionalProperties": false
        })
    }

    fn guideline(&self) -> Option<&'static str> {
        Some(
            "Use explore for focused read-only codebase research, general for a bounded implementation task in Build mode. Give each task a short name for its summary. Supply all relevant context; children see project instructions but not this conversation. Foreground calls wait by default; independent calls in the same round may run in parallel. Set background=true to receive a task ID immediately and continue other work; completion reports arrive automatically as background notifications before a later model round. Do not poll or repeat a task while it runs. If you have no more work, stop calling tools: the runtime waits for pending children and resumes you with their results before ending the turn. Give concurrent writing tasks disjoint files. Children share the workspace, not a filesystem sandbox, and cannot delegate further. Review their results before relying on them. Each child is limited to 20 model requests / 5 minutes; return only a concise final report.",
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
pub(crate) struct Task {
    agent: Kind,
    #[serde(default)]
    name: Option<String>,
    prompt: String,
    #[serde(default)]
    pub background: bool,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    resume_id: Option<String>,
}

/// Snapshot captured at the parent's turn boundary, not a reference to its history.
pub(crate) struct SubagentRunner {
    pub choice: BackendChoice,
    pub parent: Option<Arc<crate::SessionJournal>>,
    pub registry: Arc<ToolRegistry>,
    pub options: AgentOptions,
    pub resources: ResourceSnapshot,
    pub mode: Mode,
    pub slots: Semaphore,
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
    pub fn parse(&self, input: Value) -> Result<Task, ToolOutput> {
        let mut task: Task = serde_json::from_value(input).map_err(|_| {
            ToolOutput::error("subagent requires agent (explore or general) and prompt (string); optional fields are name, model, resume_id (strings) and background (boolean), with no other fields")
        })?;
        // Models sometimes fill optional string fields with empty placeholders.
        // Those carry the same meaning as omission, never a model override or resume.
        task.model = non_blank(task.model);
        task.resume_id = non_blank(task.resume_id);
        if let Some(model) = &task.model {
            if model.len() > 512 {
                return Err(ToolOutput::error(
                    "invalid child model: model ID must be at most 512 bytes; omit model to inherit the parent model",
                ));
            }
            if model.chars().any(char::is_control) {
                return Err(ToolOutput::error(
                    "invalid child model: model ID must not contain control characters; omit model to inherit the parent model",
                ));
            }
        }
        if task.prompt.trim().is_empty() || task.prompt.len() > 64 * 1024 {
            return Err(ToolOutput::error(
                "subagent prompt must be non-empty and at most 64 KiB",
            ));
        }
        if self.mode == Mode::Plan && matches!(task.agent, Kind::General) {
            return Err(ToolOutput::error(
                "Plan mode only permits explore subagents; general would allow changes",
            ));
        }
        Ok(task)
    }

    pub async fn run(
        &self,
        task: Task,
        ctx: &ToolContext,
        tx: &UnboundedSender<AgentEvent>,
        call_id: &str,
    ) -> SubagentResult {
        if ctx.is_cancelled() {
            return ToolOutput::error("subagent was cancelled before it started").into();
        }

        let mut local_ctx = ctx.clone();
        local_ctx.cancel = ctx.cancel.child();
        let _task_guard = self
            .options
            .tasks
            .register(call_id, local_ctx.cancel.clone());
        let ctx = &local_ctx;
        let totals = Arc::new(Mutex::new(Totals::default()));
        let label = format!("subagent {call_id} ({:?})", task.agent);
        let label = match task
            .name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            Some(name) => format!("{label}: {name}"),
            None => label,
        };
        let _ = tx.send(AgentEvent::Status(format!("{label}: queued")));
        // The deadline includes waiting for a shared foreground/background slot.
        let output = tokio::select! {
            biased;
            _ = crate::session::wait_for_cancel(&ctx.cancel) => ToolOutput::error("subagent was cancelled"),
            _ = tx.closed() => ToolOutput::error("subagent was cancelled"),
            _ = tokio::time::sleep(TIMEOUT) => ToolOutput::error("subagent timed out after 5 minutes"),
            output = self.execute(task, ctx, tx, call_id, Arc::clone(&totals)) => output,
        };
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

    async fn execute(
        &self,
        task: Task,
        ctx: &ToolContext,
        tx: &UnboundedSender<AgentEvent>,
        task_id: &str,
        totals: Arc<Mutex<Totals>>,
    ) -> ToolOutput {
        let Ok(_permit) = self.slots.acquire().await else {
            return ToolOutput::error("subagent execution is closed");
        };
        if ctx.is_cancelled() || tx.is_closed() {
            return ToolOutput::error("subagent was cancelled before acquiring an execution slot");
        }
        let _ = tx.send(AgentEvent::Status(format!("subagent {task_id}: running")));

        let mode = match task.agent {
            Kind::Explore => Mode::Plan,
            Kind::General => self.mode,
        };
        // An explicit allowlist/exclusion applies to the child too. Explore can
        // search/list even when Build's default declaration prefers the shell.
        let mut defaults = default_tool_names(mode).to_vec();
        let external: Vec<String> = self
            .registry
            .names()
            .into_iter()
            .filter(|name| {
                name.starts_with("mcp_")
                    && self
                        .registry
                        .get(name)
                        .is_some_and(|tool| mode == Mode::Build || tool.is_read_only())
            })
            .collect();
        defaults.extend(external.iter().map(String::as_str));
        if self.registry.get("tool_search").is_some() {
            defaults.push("tool_search");
        }
        let tools = self
            .registry
            .select(&defaults, &self.options.selection)
            .into_iter()
            .filter(|tool| tool.name() != SUBAGENT_TOOL)
            .filter(|tool| mode != Mode::Plan || tool.is_read_only())
            .collect();
        let registry = Arc::new(ToolRegistry::with_tools(tools));
        let mut choice = if let Some(model) = &task.model {
            let Some(factory) = &self.options.child_backend else {
                return ToolOutput::error(
                    "child model overrides are not configured for this runtime",
                );
            };
            match factory(model) {
                Ok(choice) => choice,
                Err(error) => return ToolOutput::error(error),
            }
        } else {
            self.choice.clone()
        };
        // Each child retains its own request ceiling while sharing the parent's budget.
        let backend = Arc::new(BoundedBackend {
            inner: Arc::clone(&choice.backend),
            requests: AtomicUsize::new(0),
            totals: Arc::clone(&totals),
            usage_tx: task.background.then(|| tx.clone()),
        });
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
        let child_model = choice.model.clone();
        let child = Agent::new(choice, registry, options);
        let journal = if let Some(parent) = &self.parent {
            let result = if let Some(id) = &task.resume_id {
                match parent.store().list(&ctx.cwd) {
                    Ok(sessions)
                        if sessions.iter().any(|session| {
                            session.id == *id && session.parent_id.as_deref() == Some(parent.id())
                        }) =>
                    {
                        parent.store().resume(id)
                    }
                    _ => {
                        return ToolOutput::error("resume_id is not a saved child of this session");
                    }
                }
            } else {
                parent
                    .store()
                    .create(&ctx.cwd, &child_model, Some(parent.id()))
            };
            match result {
                Ok(journal) => Some(journal),
                Err(error) => return ToolOutput::error(error.to_string()),
            }
        } else {
            if task.resume_id.is_some() {
                return ToolOutput::error("child resume requires a persistent parent session");
            }
            None
        };
        let session_id = journal.as_ref().map(|journal| journal.id().to_string());
        if let Some(journal) = journal
            && let Err(error) = child.attach_journal(journal)
        {
            return ToolOutput::error(error.to_string());
        }
        if let Some(session_id) = &session_id {
            let _ = tx.send(AgentEvent::SubagentSession {
                task_id: task_id.to_string(),
                session_id: session_id.clone(),
            });
        }
        let cancel = child.cancel_handle();
        let mut stream = child.run_turn(format!(
            "You are an isolated {:?} subagent. Complete only the task below and return a concise final report with findings, changed files, verification and limitations. You cannot delegate.\n\n{}",
            task.agent, task.prompt), mode);
        // A dropped parent future must also stop its child, including when the
        // child is waiting on network IO or on an approval.
        let _cancel_on_drop = CancelOnDrop(cancel.clone());
        let mut output = collect(&mut stream, ctx, tx, task_id).await;
        if let Some(session_id) = session_id {
            output.metadata.details = json!({"session_id":session_id,"model":child_model});
            output.text.push_str(&format!(
                "\n\n[subagent session: {session_id}; use resume_id to continue it explicitly]"
            ));
        }
        output
    }
}

fn non_blank(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
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
    task_id: &str,
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
                    let _ = tx.send(AgentEvent::Status(format!("subagent {task_id}: {}", call.name)));
                    let _ = tx.send(AgentEvent::SubagentToolCall {
                        task_id: task_id.to_string(),
                        call,
                    });
                }
                Some(AgentEvent::ToolResult { result, duration_ms }) => {
                    let _ = tx.send(AgentEvent::SubagentToolResult {
                        task_id: task_id.to_string(),
                        result,
                        duration_ms,
                    });
                }
                Some(AgentEvent::Error(error)) => return ToolOutput::error(format!("subagent {task_id} failed: {error}")),
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
    usage_tx: Option<UnboundedSender<AgentEvent>>,
}

#[async_trait]
impl AgentBackend for BoundedBackend {
    fn capabilities(&self) -> solaris_backend::ModelCapabilities {
        self.inner.capabilities()
    }

    async fn run_turn(&self, request: TurnRequest) -> Result<AgentEventStream, BackendError> {
        if self.requests.fetch_add(1, Ordering::Relaxed) >= MAX_CALLS {
            return Err(BackendError::new(
                "subagent exceeded its 20 model-request budget",
            ));
        }
        let stream = self.inner.run_turn(request).await?;
        let totals = Arc::clone(&self.totals);
        let usage_tx = self.usage_tx.clone();
        Ok(Box::pin(stream.map(move |event| {
            if let AgentEvent::TurnComplete { usage, cost_usd } = &event {
                let mut totals = totals
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                totals.usage = totals.usage.merge(*usage);
                totals.cost_usd += cost_usd;
                if let Some(tx) = &usage_tx {
                    let _ = tx.send(AgentEvent::SubagentUsage {
                        usage: *usage,
                        cost_usd: *cost_usd,
                    });
                }
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
            json!({"agent":"explore","prompt":"task","name":1}),
            json!({"agent":"explore","prompt":"task","background":"true"}),
            json!({"agent":"explore","prompt":"task","model":42}),
            json!({"agent":"explore","prompt":"task","resume_id":42}),
            json!({"agent":"explore","prompt":"task","unknown":true}),
            json!({"prompt":"task"}),
        ] {
            assert!(serde_json::from_value::<Task>(input).is_err());
        }
        assert_eq!(SubagentTool.parameters()["additionalProperties"], false);
    }

    #[test]
    fn delegation_accepts_a_task_name_without_requiring_it_for_older_calls() {
        let task: Task = serde_json::from_value(json!({"agent":"explore","name":"检查路由","prompt":"Inspect routing and report findings"})).unwrap();
        assert_eq!(task.name.as_deref(), Some("检查路由"));
        let task: Task =
            serde_json::from_value(json!({"agent":"explore","prompt":"legacy task"})).unwrap();
        assert!(task.name.is_none());
        assert_eq!(
            SubagentTool.parameters()["properties"]["name"]["type"],
            "string"
        );
    }

    #[test]
    fn delegation_waits_by_default_and_accepts_an_explicit_background_boolean() {
        for (input, background) in [
            (json!({"agent":"explore","prompt":"task"}), false),
            (
                json!({"agent":"general","prompt":"task","background":false}),
                false,
            ),
            (
                json!({"agent":"explore","prompt":"task","background":true}),
                true,
            ),
        ] {
            assert_eq!(
                serde_json::from_value::<Task>(input).unwrap().background,
                background
            );
        }
    }
}

/// Controls are scoped to a parent runtime and include queued tasks.
#[derive(Clone, Default)]
pub struct TaskControls(Arc<Mutex<HashMap<String, Cancel>>>);
impl TaskControls {
    pub fn active(&self) -> Vec<String> {
        let mut ids: Vec<_> = self
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .keys()
            .cloned()
            .collect();
        ids.sort();
        ids
    }
    pub fn cancel(&self, id: &str) -> bool {
        if let Some(cancel) = self.0.lock().unwrap_or_else(|p| p.into_inner()).get(id) {
            cancel.cancel();
            true
        } else {
            false
        }
    }
    fn register(&self, id: &str, cancel: Cancel) -> TaskGuard {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id.to_string(), cancel.clone());
        TaskGuard {
            controls: self.clone(),
            id: id.to_string(),
            cancel,
        }
    }
}
struct TaskGuard {
    controls: TaskControls,
    id: String,
    cancel: Cancel,
}
impl Drop for TaskGuard {
    fn drop(&mut self) {
        self.cancel.cancel();
        let mut tasks = self.controls.0.lock().unwrap_or_else(|p| p.into_inner());
        if tasks
            .get(&self.id)
            .is_some_and(|current| current.same_generation(&self.cancel))
        {
            tasks.remove(&self.id);
        }
    }
}

#[cfg(test)]
mod control_tests {
    use super::*;
    #[test]
    fn cancelling_one_task_preserves_the_parent_and_its_sibling() {
        let parent = Cancel::new().begin_turn();
        let one = parent.child();
        let two = parent.child();
        let tasks = TaskControls::default();
        let _first = tasks.register("one", one.clone());
        let _second = tasks.register("two", two.clone());
        assert!(tasks.cancel("one"));
        assert!(one.is_cancelled());
        assert!(!parent.is_cancelled());
        assert!(!two.is_cancelled());
        parent.cancel();
        assert!(two.is_cancelled());
    }
}
