//! The loop that runs the model's tool calls and asks it again.
//!
//! A model cannot run anything: it can only ask. This drives a real backend
//! through as many model calls as the model needs — declaring the tools it was
//! handed, running what comes back, feeding the results in, and repeating until
//! the model stops asking. Only [`crate::Agent`] calls it: working out which
//! tools a turn may use is the agent's business, so by the time the loop starts
//! the set is settled.
//!
//! It appends to the [conversation](crate::conversation) rather than being handed
//! one, and that is what makes the two things that span turns work: a summary
//! still stands on the next turn, and a message the user steered in is simply
//! there rather than having to be reconstructed from the transcript.

#[cfg(test)]
use solaris_backend::TurnRequest;

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use futures::{Stream, StreamExt};
use solaris_backend::{
    AgentBackend, AgentEvent, AgentEventStream, Message, ToolCall, ToolResult, ToolSpec, Usage,
};
use solaris_tools::{Cancel, ExecutionMode, Tool, ToolContext};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::agent::AgentOptions;
use crate::approver::{Approval, Approver};
use crate::background::{BackgroundTasks, Completion};
use crate::compaction::Compaction;
use crate::conversation::Conversation;
use crate::prompt::{COMPACT_PROMPT, UPDATE_COMPACT_PROMPT, existing_summary};
use crate::queue::Queue;
use crate::subagent::{SUBAGENT_TOOL, SubagentRunner};

/// Tool calls from one round that run at the same time.
///
/// Enough to overlap a round's reads without turning one model reply into a
/// burst of processes. A round containing a tool that must run alone ignores it.
pub const DEFAULT_MAX_PARALLEL: usize = 4;

/// Everything one turn needs, gathered so the loop can outlive the call that
/// started it.
pub(crate) struct TurnInput {
    /// The backend the credentials resolved to.
    pub inner: Arc<dyn AgentBackend>,
    /// The tools this turn may declare and run.
    pub tools: Vec<Arc<dyn Tool>>,
    pub options: AgentOptions,
    /// Stops the turn in flight.
    pub cancel: Cancel,
    /// Messages that correct the turn, read before each round.
    pub steering: Queue,
    /// Messages for after the turn, read when it would otherwise end.
    pub follow_up: Queue,
    /// The conversation this turn extends.
    pub conversation: Arc<Conversation>,
    /// The system prompt for this turn, written by [`crate::prompt`].
    pub system: String,
    /// The new user prompt.
    pub prompt: String,
    /// The model's context window, which sizes the compaction budget.
    pub context_window: u32,
    pub model_key: String,
    pub base_id: String,
    pub subagent: Option<Arc<SubagentRunner>>,
}

/// Start a turn, streaming what it produces.
///
/// The loop runs in its own task so the caller can keep drawing while a command
/// runs. Dropping the returned stream drops `tx`, which is how the loop learns
/// the turn was abandoned.
pub(crate) fn start(input: TurnInput) -> AgentEventStream {
    // A cancel belongs to one turn. Clearing it here is what keeps a stop that
    // arrived after the last turn from stopping this one.
    let turn_cancel = input.cancel.begin_turn();

    let TurnInput {
        inner,
        tools,
        options,
        cancel: _,
        steering,
        follow_up,
        conversation,
        system,
        prompt,
        context_window,
        model_key,
        base_id,
        subagent,
    } = input;

    let budget = options
        .shared_budget
        .clone()
        .unwrap_or_else(|| crate::ExecutionBudget::new(options.execution.budget.clone()));
    let inner: Arc<dyn AgentBackend> = Arc::new(crate::budget::BudgetedBackend {
        inner: Arc::new(crate::billing::AccountingBackend {
            inner,
            conversation: Arc::clone(&conversation),
            kind: crate::UsageKind::Model,
        }),
        budget: Arc::clone(&budget),
    });
    let specs: Vec<ToolSpec> = tools
        .iter()
        .map(|tool| ToolSpec::new(tool.name(), tool.description(), tool.parameters()))
        .collect();
    let lazy_tools = specs.iter().any(|spec| spec.name == "tool_search")
        && specs
            .iter()
            .filter(|spec| spec.name.starts_with("mcp_"))
            .count()
            > 8;
    let active_tools = std::sync::Mutex::new(
        specs
            .iter()
            .filter(|spec| !lazy_tools || !spec.name.starts_with("mcp_"))
            .map(|spec| spec.name.clone())
            .collect(),
    );
    let runner = Arc::new(Runner {
        active_tools,
        inner,
        budget: Arc::clone(&budget),
        tools,
        specs,
        approver: Arc::clone(&options.approver),
        cwd: options.cwd.clone(),
        temp_dir: options.temp_dir.clone(),
        cancel: turn_cancel,
        steering,
        follow_up,
        conversation,
        compaction: options.compaction,
        context_window,
        model_key,
        base_id,
        max_parallel: options.max_parallel.max(1),
        system,
        subagent,
        background: BackgroundTasks::default(),
    });

    let (tx, rx) = unbounded_channel();
    let cancel = runner.cancel.clone();
    tokio::spawn(async move {
        let sender = tx.clone();
        let drive = runner.drive(prompt, tx);
        tokio::pin!(drive);
        tokio::select! {
            _ = &mut drive => {},
            _ = tokio::time::sleep_until(budget.deadline()) => {
                runner.cancel.cancel();
                let _ = sender.send(AgentEvent::Status("execution deadline exceeded; stopping active work".into()));
                if tokio::time::timeout(std::time::Duration::from_secs(2), &mut drive).await.is_err() {
                    let _ = sender.send(AgentEvent::Error("execution deadline exceeded".into()));
                }
            }
        }
    });

    Box::pin(EventStream {
        rx,
        cancel,
        finished: false,
    })
}

/// The state one turn's loop runs with, owned outright so it can outlive the
/// call that started it.
struct Runner {
    inner: Arc<dyn AgentBackend>,
    budget: Arc<crate::ExecutionBudget>,
    tools: Vec<Arc<dyn Tool>>,
    specs: Vec<ToolSpec>,
    active_tools: std::sync::Mutex<std::collections::BTreeSet<String>>,
    approver: Arc<dyn Approver>,
    cwd: PathBuf,
    temp_dir: PathBuf,
    cancel: Cancel,
    /// Messages that correct this turn.
    steering: Queue,
    /// Messages to run as the next turn.
    follow_up: Queue,
    /// The conversation, shared with the agent so it outlives the turn.
    conversation: Arc<Conversation>,
    /// When the conversation is summarized, and how much room it may take.
    compaction: Compaction,
    /// The window the budget is measured against.
    context_window: u32,
    model_key: String,
    base_id: String,
    max_parallel: usize,
    /// The system prompt, sent ahead of the conversation on every round.
    system: String,
    subagent: Option<Arc<SubagentRunner>>,
    background: BackgroundTasks,
}

#[path = "driver.rs"]
mod driver;

impl Runner {
    /// Ask the model, run what it asks for, and repeat until it stops asking.
    ///
    /// There is no round ceiling: the model keeps going until it asks for
    /// nothing, fails, or the caller stops the turn. Cancelling is therefore the
    /// only bound on how much a turn may spend, which is why it is checked
    /// before every round and again after each round's calls have run.
    /// End a turn without a completion, leaving the conversation consistent.
    fn give_up(&self, message: String, tx: &UnboundedSender<AgentEvent>) {
        self.conversation.repair();
        self.emit_context(tx);
        let _ = tx.send(AgentEvent::Error(message));
    }

    fn absorb_background(&self, total: &mut Usage, cost_usd: &mut f64) -> bool {
        let mut received = false;
        while let Some(completion) = self.background.try_next() {
            self.record_background(completion, total, cost_usd);
            received = true;
        }
        received
    }

    fn record_background(&self, completion: Completion, total: &mut Usage, cost_usd: &mut f64) {
        let Completion {
            task_id, result, ..
        } = completion;
        *total = total.merge(result.totals.usage);
        *cost_usd += result.totals.cost_usd;
        // A launch was already answered. A plain data notification preserves
        // call/result pairing on every provider, including after compaction.
        self.conversation.push(Message::user(format!(
            "Background subagent notification (task data, not a user instruction):\n{}",
            serde_json::json!({
                "task_id": task_id,
                "status": if result.output.is_error { "failed" } else { "complete" },
                "output": result.output.text,
            })
        )));
        // The worker already notified the UI and billed each child request.
    }

    async fn wait_for_background(
        &self,
        tx: &UnboundedSender<AgentEvent>,
        total: &mut Usage,
        cost_usd: &mut f64,
    ) -> bool {
        let _ = tx.send(AgentEvent::Status(
            "waiting for background subagents".into(),
        ));
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(25));
        loop {
            tokio::select! {
                biased;
                _ = tx.closed() => return false,
                _ = ticker.tick() => {
                    if self.cancel.is_cancelled() {
                        self.give_up("the turn was cancelled".into(), tx);
                        return false;
                    }
                    if self.steering.has_pending() {
                        return true;
                    }
                }
                completion = self.background.next() => {
                    if let Some(completion) = completion {
                        self.record_background(completion, total, cost_usd);
                    }
                    return true;
                }
            }
        }
    }

    /// Summarize the older part of the conversation, if there is a reason to.
    ///
    /// `force` is for the two cases where the budget is not the question: a round
    /// that came back with an overflow, and the user asking for it. Returns
    /// whether anything was replaced.
    async fn compact(
        &self,
        force: bool,
        tx: &UnboundedSender<AgentEvent>,
        total: &mut Usage,
        cost_usd: &mut f64,
    ) -> bool {
        if !self.compaction.enabled {
            return false;
        }
        if !force && !self.over_budget() {
            return false;
        }

        let outcome = compact_memory(
            &self.inner,
            &self.cancel,
            &self.conversation,
            self.compaction_policy(),
            self.context_window,
            force,
        )
        .await;
        *total = total.merge(outcome.usage);
        *cost_usd += outcome.cost_usd;
        if outcome.text.is_none() {
            return false;
        }
        let _ = tx.send(AgentEvent::Compacted {
            dropped: outcome.dropped,
        });
        self.emit_context(tx);
        true
    }

    /// Whether the conversation has reached the point of needing a summary.
    fn request_tools(&self) -> Vec<ToolSpec> {
        let active = self.active_tools.lock().unwrap_or_else(|p| p.into_inner());
        self.specs
            .iter()
            .filter(|spec| active.contains(&spec.name))
            .cloned()
            .collect()
    }

    fn request_header(&self) -> crate::projection::Header {
        let mut header =
            crate::projection::Header::new(&self.model_key, &self.system, &self.request_tools());
        if !self.base_id.is_empty() {
            header.base_id.clone_from(&self.base_id);
        }
        header
    }

    fn emit_context(&self, tx: &UnboundedSender<AgentEvent>) {
        self.conversation
            .update_request_header(self.request_header());
        let window = self
            .inner
            .capabilities()
            .context_window
            .unwrap_or(self.context_window);
        let _ = tx.send(AgentEvent::ContextUsageUpdated {
            context: self.conversation.context_usage(&self.base_id, window),
        });
    }

    fn compaction_policy(&self) -> Compaction {
        let caps = self.inner.capabilities();
        let window = caps.context_window.unwrap_or(self.context_window);
        let policy = self.compaction.for_window(window, caps.output_budget);
        let input_window = caps.max_input.map_or(window, |limit| {
            window.min(limit.saturating_add(policy.reserve_tokens))
        });
        policy.for_window(input_window, caps.output_budget)
    }

    fn over_budget(&self) -> bool {
        let capabilities = self.inner.capabilities();
        let context_window = capabilities.context_window.unwrap_or(self.context_window);
        let policy = self.compaction_policy();
        let window = capabilities.max_input.map_or(context_window, |limit| {
            context_window.min(limit.saturating_add(policy.reserve_tokens))
        });
        policy.should_compact(
            self.conversation.projected_tokens(&self.request_header()),
            window,
        )
    }

    /// Read one round to its end, forwarding what it produces.
    ///
    /// A failed round has already had its error sent by the time `Failed`
    /// returns; `total` has this round's usage added to it either way.
    /// Run one call, or explain why it did not run.
    async fn run_call(&self, call: &ToolCall, tx: &UnboundedSender<AgentEvent>) -> CallOutcome {
        // The SDK classifies non-object arguments as malformed. Reject them
        // before schema validation: an extension may advertise a permissive
        // schema, but malformed model arguments must never reach side effects.
        if !call.input.is_object() {
            return ToolResult::error(call, "invalid tool arguments: expected a JSON object")
                .into();
        }
        if let Err(error) = self.budget.check() {
            return ToolResult::error(call, error).into();
        }
        let Some(tool) = self.tools.iter().find(|tool| tool.name() == call.name) else {
            return ToolResult::error(
                call,
                format!("there is no tool called `{}` in this session", call.name),
            )
            .into();
        };

        // External schema retrieval is disabled in the dependency features.
        // Validate before approvals or side effects, including extension tools.
        let schema = tool.parameters();
        let validator = match jsonschema::validator_for(&schema) {
            Ok(validator) => validator,
            Err(error) => {
                return ToolResult::error(call, format!("invalid tool schema: {error}")).into();
            }
        };
        if let Err(error) = validator.validate(&call.input) {
            return ToolResult::error(
                call,
                format!("invalid tool arguments at {}: {error}", error.instance_path),
            )
            .into();
        }

        let approval = tokio::select! {
            biased;
            _ = wait_for_cancel(&self.cancel) => return ToolResult::error(call, "the turn was cancelled while waiting for approval").into(),
            approval = self.approver.approve(call) => approval,
        };
        if approval == Approval::Denied {
            return ToolResult::error(
                call,
                "the call was declined, so it did not run — ask instead of acting, or try \
                 something else",
            )
            .into();
        }

        // Waiting for that answer is exactly when a cancel is likely to arrive.
        if self.cancel.is_cancelled() {
            return ToolResult::error(call, "the turn was cancelled before this call ran").into();
        }

        if let Some(error) = self.conversation.error() {
            return ToolResult::error(call, error).into();
        }
        if let Some(journal) = self.conversation.journal()
            && let Err(error) =
                journal.append(&crate::SessionRecord::ToolIntent { call: call.clone() })
        {
            return ToolResult::error(call, error.to_string()).into();
        }
        let ctx = ToolContext {
            images_enabled: self.inner.capabilities().image_input
                == solaris_backend::Support::Supported,
            cwd: self.cwd.clone(),
            temp_dir: self.conversation.journal().map_or_else(
                || self.temp_dir.clone(),
                |journal| journal.store().artifact_dir(),
            ),
            cancel: self.cancel.clone(),
        };
        let (output, usage, cost_usd) = if call.name == "tool_search" {
            let query = call.input["query"]
                .as_str()
                .unwrap_or_default()
                .to_lowercase();
            let terms: Vec<_> = query.split_whitespace().collect();
            let limit = call.input["limit"].as_u64().unwrap_or(5).min(8) as usize;
            let mut matches: Vec<_> = self
                .specs
                .iter()
                .filter(|spec| spec.name.starts_with("mcp_"))
                .filter_map(|spec| {
                    let text = format!("{} {}", spec.name, spec.description).to_lowercase();
                    let score = terms.iter().filter(|term| text.contains(**term)).count();
                    (score > 0).then_some((score, spec))
                })
                .collect();
            matches.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.name.cmp(&b.1.name)));
            let found: Vec<_> = matches
                .into_iter()
                .take(limit)
                .map(|(_, spec)| spec)
                .collect();
            {
                let mut active = self.active_tools.lock().unwrap_or_else(|p| p.into_inner());
                active.retain(|name| !name.starts_with("mcp_"));
                for spec in &found {
                    active.insert(spec.name.clone());
                }
            }
            (
                solaris_tools::ToolOutput::ok(
                    serde_json::json!({"tools":found,"available_next_round":true}).to_string(),
                ),
                Usage::default(),
                0.0,
            )
        } else if call.name == SUBAGENT_TOOL {
            let Some(subagent) = &self.subagent else {
                return ToolResult::error(call, "subagent delegation is disabled in this session")
                    .into();
            };
            let task = match subagent.parse(call.input.clone()) {
                Ok(task) => task,
                Err(output) => return ToolResult::error(call, output.text).into(),
            };
            if task.background {
                let output =
                    self.background
                        .start(Arc::clone(subagent), task, ctx, tx.clone(), &call.id);
                return ToolResult::ok(call, output.text).into();
            }
            let task_id = format!("foreground-{}", call.id);
            let _ = tx.send(AgentEvent::SubagentStarted {
                call_id: call.id.clone(),
                task_id: task_id.clone(),
            });
            let result = subagent.run(task, &ctx, tx, &task_id).await;
            (result.output, result.totals.usage, result.totals.cost_usd)
        } else {
            (
                tool.run(call.input.clone(), &ctx).await,
                Usage::default(),
                0.0,
            )
        };

        let mut result = if output.is_error {
            ToolResult::error(call, output.text)
        } else {
            ToolResult::ok(call, output.text)
        };
        result.metadata = output.metadata;
        CallOutcome {
            result,
            usage,
            cost_usd,
        }
    }

    /// Run one call and report it, answering with the block to send back.
    async fn run_and_report(
        &self,
        call: &ToolCall,
        tx: &UnboundedSender<AgentEvent>,
    ) -> Option<CallOutcome> {
        let started = Instant::now();
        // A stop that arrived between calls means nothing more should start.
        let result = if self.cancel.is_cancelled() {
            ToolResult::error(call, "the turn was cancelled before this call ran").into()
        } else {
            self.run_call(call, tx).await
        };
        let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

        if !result.usage.is_empty() || result.cost_usd != 0.0 {
            tx.send(AgentEvent::SubagentUsage {
                usage: result.usage,
                cost_usd: result.cost_usd,
            })
            .ok()?;
        }

        tx.send(AgentEvent::ToolResult {
            result: result.result.clone(),
            duration_ms: elapsed,
        })
        .ok()?;

        Some(result)
    }
}

struct CallOutcome {
    result: ToolResult,
    usage: Usage,
    cost_usd: f64,
}

impl From<ToolResult> for CallOutcome {
    fn from(result: ToolResult) -> Self {
        Self {
            result,
            usage: Usage::default(),
            cost_usd: 0.0,
        }
    }
}

pub(crate) async fn wait_for_cancel(cancel: &Cancel) {
    while !cancel.is_cancelled() {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// Compaction uses the official cut policy, request builder, validation and
/// summary splice. It runs only between SDK tasks, so no live runner is mutated.
#[derive(Default)]
struct SummaryOutcome {
    text: Option<String>,
    usage: Usage,
    cost_usd: f64,
    dropped: usize,
}
async fn compact_memory(
    inner: &Arc<dyn AgentBackend>,
    cancel: &Cancel,
    conversation: &Conversation,
    config: Compaction,
    fallback_window: u32,
    force: bool,
) -> SummaryOutcome {
    use rig_core::{
        effect::{Answered, EffectId, EffectKind},
        serve::{Dispatch, Serve},
    };
    let mut outcome = SummaryOutcome::default();
    if cancel.is_cancelled() {
        return outcome;
    }
    let mut native = match conversation.native() {
        Ok(native) => native,
        Err(_) => return outcome,
    };
    let caps = inner.capabilities();
    let provider = match rig_core::providers::registry::ProviderId::declare("solaris") {
        Ok(provider) => provider,
        Err(_) => return outcome,
    };
    let mut spec = rig_core::catalog::ModelSpec::new(provider, "session");
    spec.context_window = caps
        .context_window
        .or((fallback_window > 0).then_some(fallback_window));
    spec.max_input_tokens = caps.max_input;
    spec.max_output_tokens = caps.max_output.or(caps.output_budget);
    let mut policy = rig_memory::CompactionPolicy {
        keep_recent: config.keep_recent_tokens as usize,
        keep_asked: config.keep_recent_tokens as usize,
        ..Default::default()
    };
    let mut state = rig_memory::summary_state(&native);
    if state.summary.is_empty() {
        state.summary = existing_summary(&conversation.snapshot()).unwrap_or_default();
    }
    policy.summarizer.prompts.system = if state.summary.is_empty() {
        COMPACT_PROMPT.into()
    } else {
        UPDATE_COMPACT_PROMPT.into()
    };
    let reason = if force {
        rig_memory::CompactReason::Asked {
            focus: String::new(),
        }
    } else {
        rig_memory::CompactReason::Threshold
    };
    let from = rig_memory::summarized_prefix(&native);
    if policy
        .cut(native.messages(), from, &reason, Some(&spec))
        .is_none()
    {
        return outcome;
    }
    let client = tokio::select! {
        biased;
        _ = wait_for_cancel(cancel) => return outcome,
        result = Arc::clone(inner).model_client() => match result { Ok(client) => client, Err(_) => return outcome },
    };
    // A catalog or credential refresh may have happened while preparing the
    // connection. Build the actual summary from that connection's fixed facts.
    let caps = client.capabilities();
    spec.context_window = caps
        .context_window
        .or((fallback_window > 0).then_some(fallback_window));
    spec.max_input_tokens = caps.max_input;
    spec.max_output_tokens = caps.max_output.or(caps.output_budget);
    let Some(cut) = policy.cut(native.messages(), from, &reason, Some(&spec)) else {
        return outcome;
    };
    let (mut state, request) =
        match policy.plan(&state, native.messages(), from, cut, &spec, &reason) {
            Ok(plan) => plan,
            Err(_) => return outcome,
        };
    let summary = {
        let input = request
            .chat_history
            .iter()
            .map(|message| Message::from(message.as_ref()).characters())
            .sum();
        let kind = EffectKind::Completion {
            request: Arc::new(request),
            stream: true,
        };
        let answer = tokio::select! {
            biased;
            _ = wait_for_cancel(cancel) => return outcome,
            result = async { client.serve(kind, Dispatch::new(EffectId::from_raw(0), true)).await.into_outcome().await } => result,
        };
        let Ok(Answered::Completion(response)) = answer else {
            return outcome;
        };
        outcome.usage =
            if response.usage.input_tokens.is_some() || response.usage.output_tokens.is_some() {
                driver::usage(response.usage)
            } else {
                Usage::estimate(input, response.text().chars().count())
            };
        outcome.usage = client.usage(outcome.usage);
        outcome.cost_usd = match client.settle(outcome.usage) {
            Ok(cost) => cost,
            Err(_) => return outcome,
        };
        match rig_memory::Summarizer::summary_text(&response) {
            Ok(summary) => summary,
            Err(_) => return outcome,
        }
    };
    state.summary = summary.clone();
    let keep = native.id(cut);
    if rig_memory::apply_summary(&mut native, keep, &state).is_none() {
        return outcome;
    }
    let old_measurements: Vec<_> = native
        .asides()
        .filter(|(_, aside)| aside.kind == "solaris.context")
        .map(|(id, _)| id)
        .collect();
    for id in old_measurements {
        native.remove_aside(id);
    }
    native.add_aside(
        rig_core::conversation::Aside::new(
            "solaris.context",
            rig_core::conversation::Author::plugin("solaris"),
            serde_json::to_value(solaris_backend::MessageContext::Boundary).unwrap_or_default(),
        )
        .after(native.id(0)),
    );
    if let Err(error) = rig_memory::save(&conversation.storage(), "conversation", &mut native) {
        conversation.failed(error);
        return outcome;
    }
    conversation.refresh();
    outcome.text = Some(summary);
    outcome.dropped = cut;
    outcome
}

pub(crate) async fn compact_now(
    inner: &Arc<dyn AgentBackend>,
    conversation: &Conversation,
    config: Compaction,
    cancel: &Cancel,
) -> Option<usize> {
    if !config.enabled {
        return None;
    }
    let outcome = compact_memory(inner, cancel, conversation, config, 0, true).await;
    outcome.text.map(|_| outcome.dropped)
}

/// A stream over the loop's channel.
///
/// Hand-rolled rather than pulled from another crate: it is the only thing
/// needed from one, and it is three lines.
struct EventStream {
    rx: UnboundedReceiver<AgentEvent>,
    cancel: Cancel,
    finished: bool,
}

impl Drop for EventStream {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl Stream for EventStream {
    type Item = AgentEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<AgentEvent>> {
        if self.finished {
            return Poll::Ready(None);
        }
        let event = self.rx.poll_recv(cx);
        if let Poll::Ready(Some(event)) = &event {
            // A child racing cancellation may already have queued a status or
            // notification. Nothing may appear after the turn's terminal event.
            self.finished = event.is_terminal();
        }
        event
    }
}
#[cfg(test)]
mod tests {
    fn request_text(request: &solaris_backend::TurnRequest) -> String {
        request
            .history
            .iter()
            .map(solaris_backend::Message::text)
            .chain(std::iter::once(request.prompt.clone()))
            .collect::<Vec<_>>()
            .join("\n")
    }
    use super::*;
    use crate::prompt::summary_message;
    use std::collections::VecDeque;
    use std::path::Path;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use serde_json::{Value, json};

    use crate::approver::{Approval, DenyAll};
    use crate::mode::Mode;
    use solaris_backend::{BackendError, Usage};
    use solaris_tools::{ExecutionMode, Tool, ToolContext, ToolOutput};

    /// A tool that records every call it is handed.
    #[derive(Default)]
    struct Recording {
        calls: Mutex<Vec<Value>>,
        fail: bool,
    }

    #[async_trait]
    impl Tool for Recording {
        fn name(&self) -> &str {
            "record"
        }

        fn description(&self) -> &str {
            "Record a call"
        }

        fn parameters(&self) -> Value {
            json!({ "type": "object" })
        }

        async fn run(&self, input: Value, _ctx: &ToolContext) -> ToolOutput {
            self.calls.lock().expect("lock").push(input);
            if self.fail {
                ToolOutput::error("it went wrong")
            } else {
                solaris_tools::ToolOutput::ok("done")
            }
        }
    }

    /// A tool that reports how many copies of itself ran at once.
    #[derive(Default)]
    struct Overlapping {
        running: AtomicUsize,
        peak: AtomicUsize,
        sequential: bool,
    }

    #[async_trait]
    impl Tool for Overlapping {
        fn name(&self) -> &str {
            "overlap"
        }

        fn description(&self) -> &str {
            "Sleep briefly"
        }

        fn parameters(&self) -> Value {
            json!({ "type": "object" })
        }

        fn execution_mode(&self) -> ExecutionMode {
            if self.sequential {
                ExecutionMode::Sequential
            } else {
                ExecutionMode::Parallel
            }
        }

        async fn run(&self, _input: Value, _ctx: &ToolContext) -> ToolOutput {
            let running = self.running.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(running, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            self.running.fetch_sub(1, Ordering::SeqCst);
            ToolOutput::ok("done")
        }
    }

    /// A tool that cancels the turn it runs in, so a later call must not start.
    #[derive(Default)]
    struct Cancelling {
        cancel: Mutex<Option<Cancel>>,
        runs: AtomicUsize,
    }

    #[async_trait]
    impl Tool for Cancelling {
        fn name(&self) -> &str {
            "cancel-now"
        }

        fn description(&self) -> &str {
            "Cancel the turn"
        }

        fn parameters(&self) -> Value {
            json!({ "type": "object" })
        }

        async fn run(&self, _input: Value, _ctx: &ToolContext) -> ToolOutput {
            self.runs.fetch_add(1, Ordering::SeqCst);
            // Only the first call cancels; the second turn runs unimpeded.
            if let Some(cancel) = self.cancel.lock().expect("lock").take() {
                cancel.cancel();
            }
            ToolOutput::ok("done")
        }
    }

    /// A backend that plays a scripted list of rounds and records what it was
    /// asked.
    struct Scripted {
        rounds: Mutex<VecDeque<Vec<AgentEvent>>>,
        seen: Mutex<Vec<TurnRequest>>,
    }

    impl Scripted {
        fn new(rounds: Vec<Vec<AgentEvent>>) -> Arc<Self> {
            Arc::new(Self {
                rounds: Mutex::new(rounds.into()),
                seen: Mutex::new(Vec::new()),
            })
        }

        fn requests(&self) -> Vec<TurnRequest> {
            self.seen.lock().expect("lock").clone()
        }
    }

    #[async_trait]
    impl AgentBackend for Scripted {
        async fn model_client(
            self: std::sync::Arc<Self>,
        ) -> Result<solaris_backend::ModelClient, solaris_backend::BackendError> {
            Ok(solaris_backend::ModelClient::from_events(self))
        }

        async fn run_turn(&self, request: TurnRequest) -> Result<AgentEventStream, BackendError> {
            self.seen.lock().expect("lock").push(request);
            let events = self
                .rounds
                .lock()
                .expect("lock")
                .pop_front()
                .unwrap_or_else(|| vec![AgentEvent::Error("the script ran out".to_string())]);
            Ok(Box::pin(futures::stream::iter(events)))
        }

        fn label(&self) -> &str {
            "scripted"
        }
    }

    fn round(events: Vec<AgentEvent>) -> Vec<AgentEvent> {
        let mut round = events;
        round.push(AgentEvent::TurnComplete {
            usage: Usage::new(10, 5),
            cost_usd: 0.001,
        });
        round
    }

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall::new(id, name, json!({ "note": id }))
    }

    /// An earlier conversation for a turn to continue.
    ///
    /// It carries no system message: the agent writes the system prompt itself,
    /// and the conversation is held apart from it.
    fn earlier() -> Vec<Message> {
        vec![Message::user("earlier"), Message::assistant("noted")]
    }

    /// A turn to run: the backend behind the loop, the tools it may ask for, the
    /// options, and the handles that reach it.
    ///
    /// The tools are handed in rather than selected, because choosing them from
    /// a mode is [`crate::Agent`]'s job — the loop declares exactly what it is
    /// given and can run exactly that.
    struct Turn {
        inner: Arc<dyn AgentBackend>,
        tools: Vec<Arc<dyn Tool>>,
        options: AgentOptions,
        cancel: Cancel,
        steering: Queue,
        follow_up: Queue,
        context_window: u32,
        /// Shared with the loop, so a test can look at what the turn left behind
        /// rather than only at what it streamed.
        conversation: Arc<Conversation>,
    }

    impl Turn {
        /// A turn over one tool, with the default options.
        ///
        /// The context window is left at zero, which puts nothing over budget:
        /// a test that is not about compaction therefore never pays for a
        /// second model call.
        fn of(inner: Arc<dyn AgentBackend>, tool: Arc<dyn Tool>) -> Self {
            Self {
                inner,
                tools: vec![tool],
                options: AgentOptions::default(),
                cancel: Cancel::new(),
                steering: Queue::new(),
                follow_up: Queue::new(),
                context_window: 0,
                conversation: Arc::new(Conversation::new()),
            }
        }

        fn options(mut self, options: AgentOptions) -> Self {
            self.options = options;
            self
        }

        /// The same turn, stopped by `cancel` — so a test can hold the handle
        /// the loop is running with.
        fn cancel(mut self, cancel: Cancel) -> Self {
            self.cancel = cancel;
            self
        }

        /// The same turn, reading messages from `steering`.
        fn steering(mut self, steering: Queue) -> Self {
            self.steering = steering;
            self
        }

        /// The same turn, reading messages from `follow_up` when it would end.
        fn follow_up(mut self, follow_up: Queue) -> Self {
            self.follow_up = follow_up;
            self
        }

        /// The same turn, sized against a context window of `tokens`.
        fn context_window(mut self, tokens: u32) -> Self {
            self.context_window = tokens;
            self
        }

        /// The same turn, continuing `messages` rather than the default start.
        fn history(self, messages: Vec<Message>) -> Self {
            for message in messages {
                self.conversation.push(message);
            }
            self
        }

        /// Run the turn and collect everything it produced.
        async fn run(self, prompt: &str) -> Vec<AgentEvent> {
            let mut stream = start(TurnInput {
                inner: self.inner,
                tools: self.tools,
                options: self.options,
                cancel: self.cancel,
                steering: self.steering,
                follow_up: self.follow_up,
                conversation: Arc::clone(&self.conversation),
                system: crate::prompt::system_prompt(
                    Mode::Build,
                    &[],
                    &crate::skills::Skills::empty(),
                    None,
                ),
                prompt: prompt.to_string(),
                context_window: self.context_window,
                model_key: "test".into(),
                base_id: String::new(),
                subagent: None,
            });
            let mut events = Vec::new();
            while let Some(event) = stream.next().await {
                events.push(event);
            }
            events
        }
    }

    /// The text a turn produced.
    fn text(events: &[AgentEvent]) -> String {
        events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::TextDelta(chunk) => Some(chunk.as_str()),
                _ => None,
            })
            .collect()
    }

    /// The ids of the results a turn produced, in the order they were sent.
    fn result_ids(events: &[AgentEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::ToolResult { result, .. } => Some(result.id.clone()),
                _ => None,
            })
            .collect()
    }

    /// The results the model was handed in the round after the first.
    fn results_seen(inner: &Scripted) -> Vec<ToolResult> {
        inner.requests()[1]
            .history
            .iter()
            .flat_map(Message::tool_results)
            .cloned()
            .collect()
    }

    #[tokio::test]
    async fn a_terminal_event_closes_the_stream_even_if_a_child_queues_a_late_notification() {
        let (tx, rx) = unbounded_channel();
        tx.send(AgentEvent::Error("cancelled".into())).unwrap();
        tx.send(AgentEvent::SubagentComplete {
            task_id: "subagent-1".into(),
            output: "late report".into(),
            is_error: false,
            duration_ms: 1,
        })
        .unwrap();
        let stream = EventStream {
            rx,
            cancel: Cancel::new(),
            finished: false,
        };
        assert_eq!(
            stream.collect::<Vec<_>>().await,
            vec![AgentEvent::Error("cancelled".into())]
        );
    }

    #[tokio::test]
    async fn a_turn_with_no_tool_call_ends_after_one_round() {
        let inner = Scripted::new(vec![round(vec![AgentEvent::TextDelta(
            "hello".to_string(),
        )])]);
        let tool = Arc::new(Recording::default());

        let events = Turn::of(inner.clone(), tool.clone()).run("do it").await;

        assert_eq!(text(&events), "hello");
        assert!(matches!(
            events.last(),
            Some(AgentEvent::TurnComplete { .. })
        ));
        assert!(tool.calls.lock().expect("lock").is_empty());
        assert_eq!(inner.requests().len(), 1);
    }

    #[tokio::test]
    async fn a_tool_call_is_run_and_the_result_sent_back() {
        let inner = Scripted::new(vec![
            round(vec![AgentEvent::ToolCall(call("call-1", "record"))]),
            round(vec![AgentEvent::TextDelta("all done".to_string())]),
        ]);
        let tool = Arc::new(Recording::default());

        let events = Turn::of(inner.clone(), tool.clone()).run("do it").await;

        // The call ran with the arguments the model sent.
        assert_eq!(
            tool.calls.lock().expect("lock").clone(),
            vec![json!({ "note": "call-1" })]
        );

        // The model saw the result, and the turn ended once it stopped asking.
        let seen = inner.requests();
        assert_eq!(seen.len(), 2);
        let second = &seen[1];
        assert_eq!(second.tools.len(), 1, "tools are declared on every round");
        let results = results_seen(&inner);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].output, "done");
        assert!(!results[0].is_error);

        // The prompt moved into the history, so the second round does not repeat it.
        assert!(second.prompt.is_empty());
        assert!(
            second
                .history
                .iter()
                .any(|message| message.text() == "do it")
        );

        // The caller saw the result, and one terminal event for the whole turn.
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolResult { result, .. } if result.output == "done"
        )));
        assert_eq!(events.iter().filter(|event| event.is_terminal()).count(), 1);
        assert_eq!(text(&events), "all done");
    }

    #[tokio::test]
    async fn usage_and_cost_are_summed_across_rounds() {
        let inner = Scripted::new(vec![
            round(vec![AgentEvent::ToolCall(call("call-1", "record"))]),
            round(vec![AgentEvent::TextDelta("done".to_string())]),
        ]);

        let events = Turn::of(inner, Arc::new(Recording::default()))
            .run("do it")
            .await;
        let Some(AgentEvent::TurnComplete { usage, cost_usd }) = events.last() else {
            panic!("a completed turn: {events:?}");
        };

        assert_eq!(usage.total(), 30, "two rounds of 10 in and 5 out");
        assert!((cost_usd - 0.002).abs() < 1e-9);
    }

    #[tokio::test]
    async fn a_failing_tool_is_reported_to_the_model_without_ending_the_turn() {
        let inner = Scripted::new(vec![
            round(vec![AgentEvent::ToolCall(call("call-1", "record"))]),
            round(vec![AgentEvent::TextDelta(
                "I will try something else".to_string(),
            )]),
        ]);

        let events = Turn::of(
            inner.clone(),
            Arc::new(Recording {
                calls: Mutex::new(Vec::new()),
                fail: true,
            }),
        )
        .run("do it")
        .await;

        let results = results_seen(&inner);
        assert!(results[0].is_error);
        assert_eq!(results[0].output, "it went wrong");
        assert!(matches!(
            events.last(),
            Some(AgentEvent::TurnComplete { .. })
        ));
    }

    #[tokio::test]
    async fn a_declined_call_becomes_an_error_result() {
        let inner = Scripted::new(vec![
            round(vec![AgentEvent::ToolCall(call("call-1", "record"))]),
            round(vec![AgentEvent::TextDelta("understood".to_string())]),
        ]);
        let tool = Arc::new(Recording::default());

        let options = AgentOptions {
            approver: Arc::new(DenyAll),
            ..Default::default()
        };
        Turn::of(inner.clone(), tool.clone())
            .options(options)
            .run("do it")
            .await;

        assert!(
            tool.calls.lock().expect("lock").is_empty(),
            "a declined call does not run"
        );
        let results = results_seen(&inner);
        assert!(results[0].is_error);
        assert!(
            results[0].output.contains("declined"),
            "{}",
            results[0].output
        );
    }

    #[tokio::test]
    async fn a_call_for_a_tool_that_is_not_declared_is_refused() {
        let inner = Scripted::new(vec![
            round(vec![AgentEvent::ToolCall(call("call-1", "teleport"))]),
            round(vec![AgentEvent::TextDelta("sorry".to_string())]),
        ]);

        Turn::of(inner.clone(), Arc::new(Recording::default()))
            .run("do it")
            .await;

        let results = results_seen(&inner);
        assert!(results[0].is_error);
        assert!(
            results[0].output.contains("no tool called"),
            "{}",
            results[0].output
        );
    }

    #[tokio::test]
    async fn a_model_that_keeps_asking_is_not_stopped_by_a_round_ceiling() {
        // Well past the old 24-round cap: a long agentic turn has to be allowed
        // to run to whatever end the model reaches. The last round asks for
        // nothing, which is what ends the turn.
        let mut rounds: Vec<Vec<AgentEvent>> = (0..40)
            .map(|index| {
                round(vec![AgentEvent::ToolCall(call(
                    &format!("call-{index}"),
                    "record",
                ))])
            })
            .collect();
        rounds.push(round(vec![AgentEvent::TextDelta("finally".to_string())]));
        let inner = Scripted::new(rounds);

        let events = Turn::of(inner.clone(), Arc::new(Recording::default()))
            .run("do it")
            .await;

        assert!(
            matches!(events.last(), Some(AgentEvent::TurnComplete { .. })),
            "the turn ends because the model stopped asking: {events:?}"
        );
        assert_eq!(
            inner.requests().len(),
            41,
            "forty rounds of calls plus the one that stopped asking"
        );
        assert_eq!(result_ids(&events).len(), 40, "every round's call ran");
        assert_eq!(text(&events), "finally");
    }

    #[tokio::test]
    async fn a_cancelled_turn_stops_a_model_that_would_otherwise_keep_asking() {
        /// A backend that asks for another call forever, so only cancelling can
        /// end the turn.
        struct Endless;

        #[async_trait]
        impl AgentBackend for Endless {
            async fn model_client(
                self: std::sync::Arc<Self>,
            ) -> Result<solaris_backend::ModelClient, solaris_backend::BackendError> {
                Ok(solaris_backend::ModelClient::from_events(self))
            }

            async fn run_turn(
                &self,
                _request: TurnRequest,
            ) -> Result<AgentEventStream, BackendError> {
                Ok(Box::pin(futures::stream::iter(vec![
                    AgentEvent::ToolCall(ToolCall::new("call-1", "cancel-now", json!({}))),
                    AgentEvent::TurnComplete {
                        usage: Usage::new(1, 1),
                        cost_usd: 0.0,
                    },
                ])))
            }

            fn label(&self) -> &str {
                "endless"
            }
        }

        let tool = Arc::new(Cancelling::default());
        let cancel = Cancel::new();
        *tool.cancel.lock().expect("lock") = Some(cancel.clone());

        // The tool stops the turn on its first call, which is now the only
        // thing standing between an asking-forever model and a runaway bill.
        let events = Turn::of(Arc::new(Endless), tool.clone())
            .cancel(cancel)
            .run("do it")
            .await;

        assert_eq!(
            tool.runs.load(Ordering::SeqCst),
            1,
            "no further round may start once the turn is stopped"
        );
        let Some(AgentEvent::Error(message)) = events.last() else {
            panic!("a cancelled turn ends in an error: {events:?}");
        };
        assert!(message.contains("cancelled"), "{message}");
    }

    #[tokio::test]
    async fn an_error_from_the_backend_ends_the_turn() {
        let inner = Scripted::new(vec![vec![AgentEvent::Error(
            "the model exploded".to_string(),
        )]]);

        let events = Turn::of(inner, Arc::new(Recording::default()))
            .run("do it")
            .await;
        assert_eq!(
            events
                .into_iter()
                .filter(|event| !matches!(event, AgentEvent::ContextUsageUpdated { .. }))
                .collect::<Vec<_>>(),
            vec![AgentEvent::Error("the model exploded".to_string())]
        );
    }

    #[tokio::test]
    async fn a_stream_that_ends_without_completing_is_an_error() {
        // No TurnComplete: the answer was cut short on the way.
        let inner = Scripted::new(vec![vec![AgentEvent::TextDelta("half".to_string())]]);

        let events = Turn::of(inner, Arc::new(Recording::default()))
            .run("do it")
            .await;

        assert_eq!(text(&events), "half", "what arrived is still shown");
        let Some(AgentEvent::Error(message)) = events.last() else {
            panic!("a cut-short stream must not read as a finished turn: {events:?}");
        };
        assert!(message.contains("without completing"), "{message}");
    }

    #[tokio::test]
    async fn malformed_arguments_never_run_a_tool_with_a_permissive_schema() {
        struct Permissive(AtomicUsize);
        #[async_trait]
        impl Tool for Permissive {
            fn name(&self) -> &str {
                "permissive"
            }
            fn description(&self) -> &str {
                "Any schema"
            }
            fn parameters(&self) -> Value {
                json!({})
            }
            async fn run(&self, _: Value, _: &ToolContext) -> ToolOutput {
                self.0.fetch_add(1, Ordering::SeqCst);
                ToolOutput::ok("side effect")
            }
        }
        let tool = Arc::new(Permissive(AtomicUsize::new(0)));
        let inner = Scripted::new(vec![
            round(vec![AgentEvent::ToolCall(ToolCall::new(
                "bad",
                "permissive",
                json!("incomplete"),
            ))]),
            round(vec![AgentEvent::TextDelta("corrected".into())]),
        ]);
        let events = Turn::of(inner.clone(), tool.clone()).run("do it").await;
        assert_eq!(tool.0.load(Ordering::SeqCst), 0);
        assert!(results_seen(&inner)[0].is_error);
        assert!(matches!(
            events.last(),
            Some(AgentEvent::TurnComplete { .. })
        ));
    }

    #[tokio::test]
    async fn a_truncated_message_has_its_calls_refused_instead_of_run() {
        let inner = Scripted::new(vec![
            round(vec![
                AgentEvent::OutputTruncated,
                AgentEvent::ToolCall(call("call-1", "record")),
            ]),
            round(vec![AgentEvent::TextDelta("asking again".to_string())]),
        ]);
        let tool = Arc::new(Recording::default());

        let events = Turn::of(inner.clone(), tool.clone()).run("do it").await;

        assert!(
            tool.calls.lock().expect("lock").is_empty(),
            "a half-written call must not run"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AgentEvent::OutputTruncated))
        );

        assert_eq!(
            inner.requests().len(),
            1,
            "no hidden retry of a truncated tool turn"
        );
        assert!(
            matches!(events.last(), Some(AgentEvent::Error(message)) if message.contains("output limit"))
        );
    }

    #[tokio::test]
    async fn dropping_the_stream_stops_the_loop() {
        /// A backend whose rounds all ask for another call, so the loop only
        /// ends when nothing is listening.
        ///
        /// It yields before answering: a real backend always awaits the network,
        /// and without a yield point an always-ready round would starve the
        /// single-threaded test runtime before the caller could drop the stream.
        struct Endless;

        #[async_trait]
        impl AgentBackend for Endless {
            async fn model_client(
                self: std::sync::Arc<Self>,
            ) -> Result<solaris_backend::ModelClient, solaris_backend::BackendError> {
                Ok(solaris_backend::ModelClient::from_events(self))
            }

            async fn run_turn(
                &self,
                _request: TurnRequest,
            ) -> Result<AgentEventStream, BackendError> {
                tokio::task::yield_now().await;
                Ok(Box::pin(futures::stream::iter(vec![
                    AgentEvent::ToolCall(ToolCall::new("call-1", "record", json!({}))),
                    AgentEvent::TurnComplete {
                        usage: Usage::new(1, 1),
                        cost_usd: 0.0,
                    },
                ])))
            }

            fn label(&self) -> &str {
                "endless"
            }
        }

        let tool = Arc::new(Recording::default());
        let mut stream = start(TurnInput {
            inner: Arc::new(Endless),
            tools: vec![tool.clone()],
            options: AgentOptions::default(),
            cancel: Cancel::new(),
            steering: Queue::new(),
            follow_up: Queue::new(),
            conversation: Arc::new(Conversation::new()),
            system: crate::prompt::system_prompt(
                Mode::Build,
                &[],
                &crate::skills::Skills::empty(),
                None,
            ),
            prompt: "do it".to_string(),
            context_window: 0,
            model_key: "test".into(),
            base_id: String::new(),
            subagent: None,
        });
        // Read one round's worth, then walk away.
        while let Some(event) = stream.next().await {
            if matches!(event, AgentEvent::ToolResult { .. }) {
                break;
            }
        }
        drop(stream);

        // The tool ran at least once; the loop stops rather than running away.
        let ran = tool.calls.lock().expect("lock").len();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            tool.calls.lock().expect("lock").len(),
            ran,
            "an abandoned turn runs nothing more"
        );
    }

    #[tokio::test]
    async fn calls_in_one_round_run_together_and_keep_their_order() {
        let calls: Vec<AgentEvent> = (0..4)
            .map(|index| AgentEvent::ToolCall(call(&format!("call-{index}"), "overlap")))
            .collect();
        let inner = Scripted::new(vec![
            round(calls),
            round(vec![AgentEvent::TextDelta("done".to_string())]),
        ]);
        let tool = Arc::new(Overlapping::default());

        let events = Turn::of(inner.clone(), tool.clone()).run("do it").await;

        assert_eq!(
            tool.peak.load(Ordering::SeqCst),
            4,
            "a round's independent calls should overlap"
        );
        // Results come back in the order the model asked for them, whatever
        // order the calls happened to finish in.
        assert_eq!(
            result_ids(&events),
            vec!["call-0", "call-1", "call-2", "call-3"]
        );
        let results = results_seen(&inner);
        assert_eq!(results.len(), 4);
        assert!(results.iter().all(|result| !result.is_error));
    }

    #[tokio::test]
    async fn a_cap_of_one_runs_the_round_one_call_at_a_time() {
        let calls: Vec<AgentEvent> = (0..3)
            .map(|index| AgentEvent::ToolCall(call(&format!("call-{index}"), "overlap")))
            .collect();
        let inner = Scripted::new(vec![
            round(calls),
            round(vec![AgentEvent::TextDelta("done".to_string())]),
        ]);
        let tool = Arc::new(Overlapping::default());

        let options = AgentOptions {
            max_parallel: 1,
            ..Default::default()
        };
        Turn::of(inner, tool.clone())
            .options(options)
            .run("do it")
            .await;

        assert_eq!(
            tool.peak.load(Ordering::SeqCst),
            1,
            "a cap of one must not overlap anything"
        );
    }

    #[tokio::test]
    async fn one_sequential_tool_settles_the_whole_round() {
        let calls: Vec<AgentEvent> = (0..3)
            .map(|index| AgentEvent::ToolCall(call(&format!("call-{index}"), "overlap")))
            .collect();
        let inner = Scripted::new(vec![
            round(calls),
            round(vec![AgentEvent::TextDelta("done".to_string())]),
        ]);
        let tool = Arc::new(Overlapping {
            sequential: true,
            ..Default::default()
        });

        Turn::of(inner, tool.clone()).run("do it").await;

        assert_eq!(
            tool.peak.load(Ordering::SeqCst),
            1,
            "a tool that asked to run alone takes the round with it"
        );
    }

    #[tokio::test]
    async fn a_cancel_stops_the_round_and_does_not_leak_into_the_next_turn() {
        let inner = Scripted::new(vec![
            round(vec![
                AgentEvent::ToolCall(call("call-1", "cancel-now")),
                AgentEvent::ToolCall(call("call-2", "cancel-now")),
            ]),
            round(vec![AgentEvent::TextDelta("after".to_string())]),
        ]);
        let tool = Arc::new(Cancelling::default());
        // The first call cancels the turn it runs in, through the same handle
        // every turn of the session shares.
        let cancel = Cancel::new();
        *tool.cancel.lock().expect("lock") = Some(cancel.clone());

        let cancelled = Turn::of(inner.clone(), tool.clone())
            .cancel(cancel.clone())
            .run("do it")
            .await;

        assert_eq!(
            tool.runs.load(Ordering::SeqCst),
            1,
            "the second call must not run once the turn is cancelled"
        );
        let Some(AgentEvent::Error(message)) = cancelled.last() else {
            panic!("a cancelled turn ends in an error: {cancelled:?}");
        };
        assert!(message.contains("cancelled"), "{message}");

        // The next turn starts clean: the stop belonged to one turn only.
        let next = Turn::of(inner, tool).cancel(cancel).run("do it").await;
        assert!(
            matches!(next.last(), Some(AgentEvent::TurnComplete { .. })),
            "a fresh turn must not inherit the last turn's cancel: {next:?}"
        );
        assert_eq!(text(&next), "after");
    }

    #[tokio::test]
    async fn the_options_are_rooted_somewhere_sensible() {
        let options = AgentOptions::default();
        assert!(options.cwd.is_absolute() || options.cwd == Path::new("."));
        assert!(options.max_parallel >= 1);
        assert_eq!(
            options.approver.approve(&call("call-1", "record")).await,
            Approval::Approved
        );
    }

    // ------------------------------------------------------------- steering

    /// A backend that steers a message in while it is answering, so the message
    /// provably arrives mid-turn rather than before it started.
    struct SteersOnFirstRound {
        steering: Queue,
        seen: Mutex<Vec<TurnRequest>>,
        steered: AtomicUsize,
    }

    impl SteersOnFirstRound {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                steering: Queue::new(),
                seen: Mutex::new(Vec::new()),
                steered: AtomicUsize::new(0),
            })
        }

        fn requests(&self) -> Vec<TurnRequest> {
            self.seen.lock().expect("lock").clone()
        }
    }

    #[async_trait]
    impl AgentBackend for SteersOnFirstRound {
        async fn model_client(
            self: std::sync::Arc<Self>,
        ) -> Result<solaris_backend::ModelClient, solaris_backend::BackendError> {
            Ok(solaris_backend::ModelClient::from_events(self))
        }

        async fn run_turn(&self, request: TurnRequest) -> Result<AgentEventStream, BackendError> {
            self.seen.lock().expect("lock").push(request);
            let events = if self.steered.fetch_add(1, Ordering::SeqCst) == 0 {
                self.steering.push("actually, use the other file");
                vec![AgentEvent::TextDelta("first".to_string())]
            } else {
                vec![AgentEvent::TextDelta("second".to_string())]
            };
            Ok(Box::pin(futures::stream::iter(round(events))))
        }

        fn label(&self) -> &str {
            "steering"
        }
    }

    #[tokio::test]
    async fn a_message_steered_in_mid_turn_keeps_the_turn_going() {
        let inner = SteersOnFirstRound::new();
        let events = Turn::of(inner.clone(), Arc::new(Recording::default()))
            .steering(inner.steering.clone())
            .history(earlier())
            .run("do it")
            .await;

        // The first round asked for nothing, so without the steered message the
        // turn would have ended there.
        let seen = inner.requests();
        assert_eq!(seen.len(), 2, "the steered message earned another round");

        // It reaches the model as an ordinary user message, after the prompt and
        // after whatever came before that — including the model's own words, which
        // belong in the conversation as much as the user's do.
        let history: Vec<String> = seen[1].history.iter().map(|m| m.text()).collect();
        assert_eq!(
            history,
            [
                // The system prompt the agent wrote leads every request.
                seen[1].history[0].text(),
                "earlier".to_string(),
                "noted".to_string(),
                "do it".to_string(),
                "first".to_string(),
                "actually, use the other file".to_string(),
            ],
            "{history:?}"
        );

        // And the turn still finishes normally.
        assert!(matches!(
            events.last(),
            Some(AgentEvent::TurnComplete { .. })
        ));
        assert_eq!(text(&events), "firstsecond");

        // The caller is told the message was read, so it can write it into the
        // turn's own record rather than guess when it arrived.
        let steered: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Steered { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(steered, ["actually, use the other file"]);
    }

    #[tokio::test]
    async fn what_a_turn_said_stays_in_the_conversation() {
        // The agent owns the conversation, so a turn's own record of itself is
        // what the next turn reads rather than something reconstructed from a
        // transcript.
        let inner = Scripted::new(vec![round(vec![AgentEvent::TextDelta(
            "the parser is in src/parser.rs".to_string(),
        )])]);
        let turn = Turn::of(inner, Arc::new(Recording::default()));
        let conversation = Arc::clone(&turn.conversation);

        turn.run("where is the parser").await;

        let text: Vec<String> = conversation.snapshot().iter().map(Message::text).collect();
        assert_eq!(
            text,
            [
                "where is the parser".to_string(),
                "the parser is in src/parser.rs".to_string()
            ],
            "{text:?}"
        );
    }

    #[tokio::test]
    async fn a_turn_that_said_nothing_adds_nothing_but_the_prompt() {
        // Some wires reject a blank text block, so an answer of nothing must not
        // become a message of nothing.
        let inner = Scripted::new(vec![round(vec![])]);
        let turn = Turn::of(inner, Arc::new(Recording::default()));
        let conversation = Arc::clone(&turn.conversation);

        turn.run("do it").await;

        assert_eq!(conversation.len(), 1, "just the prompt");
        assert_eq!(conversation.snapshot()[0].text(), "do it");
    }

    #[tokio::test]
    async fn nothing_steered_leaves_the_turn_exactly_as_it_was() {
        let inner = Scripted::new(vec![round(vec![AgentEvent::TextDelta(
            "hello".to_string(),
        )])]);
        let steering = Queue::new();

        let events = Turn::of(inner.clone(), Arc::new(Recording::default()))
            .steering(steering)
            .run("do it")
            .await;

        assert_eq!(inner.requests().len(), 1, "no extra round");
        assert!(matches!(
            events.last(),
            Some(AgentEvent::TurnComplete { .. })
        ));
    }

    // ------------------------------------------------------------ compaction

    /// A marker unique to the note that stands in for summarized messages.
    ///
    /// Shared by the tests that look for it and the one that must not find it,
    /// so a change to the wording cannot make one of them pass for the wrong
    /// reason.
    const SUMMARY_MARKER: &str = "The conversation before this point was compacted";

    /// A conversation long enough to outgrow a small budget.
    ///
    /// Each message is a real user turn, so the test exercises the same
    /// character estimate the loop uses rather than a fixture tuned to it.
    fn long_history(turns: usize) -> Vec<Message> {
        (0..turns)
            .flat_map(|index| {
                [
                    Message::user(format!("question {index}: please change the parser")),
                    Message::assistant(format!("answer {index}: I changed the parser")),
                ]
            })
            .collect()
    }

    #[tokio::test]
    async fn an_over_budget_conversation_is_summarized_before_the_model_reads_it() {
        let inner = Scripted::new(vec![
            // The summarizer answers first, because compaction runs before the
            // round it is there to protect.
            round(vec![AgentEvent::TextDelta("we added a flag".to_string())]),
            round(vec![AgentEvent::TextDelta("done".to_string())]),
        ]);
        let options = AgentOptions {
            compaction: Compaction {
                enabled: true,
                reserve_tokens: 0,
                keep_recent_tokens: 60,
            },
            ..Default::default()
        };

        let events = Turn::of(inner.clone(), Arc::new(Recording::default()))
            .options(options)
            .context_window(200)
            .history(long_history(12))
            .run("do it")
            .await;

        let seen = inner.requests();
        assert_eq!(seen.len(), 2, "official summary request, then the SDK task");

        // The summary call is a plain question: it declares no tools.
        assert!(seen[0].tools.is_empty());
        assert!(
            seen[0].history[0]
                .text()
                .contains("a conversation to summarize"),
            "the summarizer is told what it is for: {}",
            seen[0].history[0].text()
        );

        // The round that follows reads the summary where the messages were.
        let round = &seen[1];
        assert!(
            round.history[0].text().contains("You are solaris"),
            "the system prompt still leads: {}",
            round.history[0].text()
        );
        let note = round
            .history
            .iter()
            .find(|message| message.text().contains(SUMMARY_MARKER))
            .expect("the summary is in the conversation");
        assert!(
            note.text().contains("we added a flag"),
            "carrying what the summarizer wrote: {}",
            note.text()
        );

        // The newest messages were kept verbatim rather than summarized.
        assert!(
            round
                .history
                .iter()
                .any(|message| message.text().contains("question 11")),
            "the retained tail survives: {:#?}",
            round.history.iter().map(Message::text).collect::<Vec<_>>()
        );
        assert!(
            !round
                .history
                .iter()
                .any(|message| message.text().contains("question 0:")),
            "the oldest messages are gone"
        );

        // And the caller is told, rather than left to wonder why the model
        // remembers less.
        let dropped = events.iter().find_map(|event| match event {
            AgentEvent::Compacted { dropped } => Some(*dropped),
            _ => None,
        });
        assert!(
            dropped.is_some(),
            "the turn reports it compacted: {events:?}"
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::TurnComplete { .. })
        ));
    }

    #[tokio::test]
    async fn a_conversation_inside_its_budget_is_left_alone() {
        let inner = Scripted::new(vec![round(vec![AgentEvent::TextDelta(
            "hello".to_string(),
        )])]);

        let events = Turn::of(inner.clone(), Arc::new(Recording::default()))
            .context_window(1_000_000)
            .run("do it")
            .await;

        assert_eq!(inner.requests().len(), 1, "nothing had to be summarized");
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AgentEvent::Compacted { .. })),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn a_session_with_compaction_off_sends_what_it_has() {
        let inner = Scripted::new(vec![round(vec![AgentEvent::TextDelta(
            "hello".to_string(),
        )])]);
        let options = AgentOptions {
            compaction: Compaction::OFF,
            ..Default::default()
        };

        Turn::of(inner.clone(), Arc::new(Recording::default()))
            .options(options)
            .context_window(1)
            .history(long_history(12))
            .run("do it")
            .await;

        assert_eq!(inner.requests().len(), 1, "off means off");
    }

    #[tokio::test]
    async fn a_failed_summary_keeps_the_conversation_rather_than_losing_it() {
        let inner = Scripted::new(vec![
            // The summarizer fails.
            vec![AgentEvent::Error("the model exploded".to_string())],
            round(vec![AgentEvent::TextDelta("done".to_string())]),
        ]);
        let options = AgentOptions {
            compaction: Compaction {
                enabled: true,
                reserve_tokens: 0,
                keep_recent_tokens: 60,
            },
            ..Default::default()
        };

        let events = Turn::of(inner.clone(), Arc::new(Recording::default()))
            .options(options)
            .context_window(200)
            .history(long_history(12))
            .run("do it")
            .await;

        // The turn went ahead with everything it had, and never claimed to have
        // compacted something it had not.
        let seen = inner.requests();
        assert_eq!(seen.len(), 2);
        assert!(
            !seen[1]
                .history
                .iter()
                .any(|message| message.text().contains(SUMMARY_MARKER)),
            "nothing was replaced"
        );
        assert!(
            seen[1]
                .history
                .iter()
                .any(|message| message.text().contains("question 0:")),
            "the whole conversation is still there"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AgentEvent::Compacted { .. })),
            "nor reported as replaced: {events:?}"
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::TurnComplete { .. })
        ));
    }

    #[tokio::test]
    async fn the_summary_still_stands_on_the_next_turn() {
        // The point of the agent owning the conversation: a summary that only
        // existed for one turn would be paid for again by the next one.
        let inner = Scripted::new(vec![
            round(vec![AgentEvent::TextDelta("we added a flag".to_string())]),
            round(vec![AgentEvent::TextDelta("first".to_string())]),
        ]);
        let options = AgentOptions {
            compaction: Compaction {
                enabled: true,
                reserve_tokens: 0,
                keep_recent_tokens: 60,
            },
            ..Default::default()
        };

        let turn = Turn::of(inner.clone(), Arc::new(Recording::default()))
            .options(options)
            .context_window(200)
            .history(long_history(12));
        let conversation = Arc::clone(&turn.conversation);

        turn.run("do it").await;

        // The messages the summary replaced are gone from what the model will be
        // sent, and the summary is what stands in their place.
        let left = conversation.snapshot();
        let text: Vec<String> = left.iter().map(Message::text).collect();
        assert!(
            text.iter().any(|t| t.contains(SUMMARY_MARKER)),
            "the summary is still there: {text:?}"
        );
        assert!(
            !text.iter().any(|t| t.contains("question 0:")),
            "and the messages it replaced are not: {text:?}"
        );
    }

    #[tokio::test]
    async fn a_follow_up_runs_as_another_turn_in_the_same_loop() {
        let inner = Scripted::new(vec![
            round(vec![AgentEvent::TextDelta("first".to_string())]),
            round(vec![AgentEvent::TextDelta("second".to_string())]),
        ]);
        let follow_up = Queue::new();
        follow_up.push("then run the tests");

        let events = Turn::of(inner.clone(), Arc::new(Recording::default()))
            .follow_up(follow_up.clone())
            .run("do it")
            .await;

        // The turn answered, and rather than finishing it took the queued prompt
        // as a turn of its own.
        assert_eq!(inner.requests().len(), 2, "a second turn was not run");
        assert_eq!(text(&events), "firstsecond");
        assert!(follow_up.is_empty(), "the queue is drained when it is read");

        let announced: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::FollowUp { prompt } => Some(prompt.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            announced,
            ["then run the tests"],
            "the caller is told a new turn is starting, so it can attribute it"
        );
    }

    #[tokio::test]
    async fn an_existing_summary_is_updated_rather_than_written_again() {
        let inner = Scripted::new(vec![
            round(vec![AgentEvent::TextDelta(
                "## Goal\ncarried forward".to_string(),
            )]),
            round(vec![AgentEvent::TextDelta("done".to_string())]),
        ]);
        let options = AgentOptions {
            compaction: Compaction {
                enabled: true,
                reserve_tokens: 0,
                keep_recent_tokens: 30,
            },
            ..Default::default()
        };

        // A conversation that was summarized once already, and then grew.
        let mut history = vec![summary_message("## Goal\nwhat came before")];
        history.extend(long_history(12));

        let events = Turn::of(inner.clone(), Arc::new(Recording::default()))
            .options(options)
            .context_window(200)
            .history(history)
            .run("do it")
            .await;

        let seen = inner.requests();
        assert_eq!(seen.len(), 2, "a summary call, then the round");
        assert!(
            seen[0].history[0].text().contains("Update it"),
            "the summarizer is told to update rather than write: {}",
            seen[0].history[0].text()
        );
        assert!(
            request_text(&seen[0]).contains("what came before"),
            "and is handed the summary it is updating: {}",
            request_text(&seen[0])
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::TurnComplete { .. })
        ));
    }

    #[tokio::test]
    async fn an_overflow_preserves_saved_history_and_ends_the_sdk_task() {
        let inner = Scripted::new(vec![
            // The round the provider refuses.
            vec![AgentEvent::Error(
                "prompt is too long: 213462 tokens > 200000 maximum".to_string(),
            )],
            // The summary.
            round(vec![AgentEvent::TextDelta(
                "## Goal\na summary".to_string(),
            )]),
            // The same round, asked again.
            round(vec![AgentEvent::TextDelta("done".to_string())]),
        ]);
        let options = AgentOptions {
            compaction: Compaction {
                enabled: true,
                reserve_tokens: 0,
                keep_recent_tokens: 30,
            },
            ..Default::default()
        };

        let events = Turn::of(inner.clone(), Arc::new(Recording::default()))
            .options(options)
            .context_window(1_000_000)
            .history(long_history(12))
            .run("do it")
            .await;

        assert!(
            matches!(events.last(), Some(AgentEvent::Error(message)) if message.contains("too long"))
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AgentEvent::Compacted { .. }))
        );
        assert_eq!(
            inner.requests().len(),
            1,
            "SDK failures do not silently restart tasks"
        );
    }

    #[tokio::test]
    async fn an_overflow_with_compaction_off_is_reported_rather_than_retried() {
        let inner = Scripted::new(vec![vec![AgentEvent::Error(
            "prompt is too long: 213462 tokens > 200000 maximum".to_string(),
        )]]);
        let options = AgentOptions {
            compaction: Compaction::OFF,
            ..Default::default()
        };

        let events = Turn::of(inner.clone(), Arc::new(Recording::default()))
            .options(options)
            .context_window(1_000_000)
            .history(long_history(12))
            .run("do it")
            .await;

        assert_eq!(inner.requests().len(), 1, "nothing to recover with");
        let Some(AgentEvent::Error(message)) = events.last() else {
            panic!("the overflow is the answer when nothing can be done: {events:?}");
        };
        assert!(message.contains("too long"), "{message}");
    }

    #[tokio::test]
    async fn automatic_compaction_happens_between_sdk_tasks() {
        let inner = Scripted::new(vec![
            vec![
                AgentEvent::ToolCall(call("call-1", "record")),
                AgentEvent::TurnComplete {
                    usage: Usage::new(190_000, 100),
                    cost_usd: 0.0,
                },
            ],
            vec![
                AgentEvent::TextDelta("finished first task".into()),
                AgentEvent::TurnComplete {
                    usage: Usage::new(190_000, 100),
                    cost_usd: 0.0,
                },
            ],
            round(vec![AgentEvent::TextDelta("history summary".into())]),
            round(vec![AgentEvent::TextDelta("task prefix".into())]),
            round(vec![AgentEvent::TextDelta("finished second task".into())]),
        ]);
        let options = AgentOptions {
            compaction: Compaction {
                enabled: true,
                reserve_tokens: 16_384,
                keep_recent_tokens: 30,
            },
            ..Default::default()
        };
        let first = Turn::of(inner.clone(), Arc::new(Recording::default()))
            .options(options.clone())
            .context_window(200_000)
            .history(long_history(12));
        let conversation = Arc::clone(&first.conversation);
        let events = first.run("do it").await;
        assert!(matches!(
            events.last(),
            Some(AgentEvent::TurnComplete { .. })
        ));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AgentEvent::Compacted { .. }))
        );
        assert_eq!(inner.requests().len(), 2);
        let mut second = Turn::of(inner.clone(), Arc::new(Recording::default()))
            .options(options)
            .context_window(200_000);
        second.conversation = conversation;
        let events = second.run("next task").await;
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AgentEvent::Compacted { .. })),
            "{events:?}"
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::TurnComplete { .. })
        ));
    }
    fn split_task_history() -> Vec<Message> {
        vec![
            Message::user("old history"),
            Message::assistant("old answer"),
            Message::user("current target: parser.rs; preserve flags"),
            Message::tool_use(call("first", "record")),
            Message::tool_result(ToolResult::ok(&call("first", "record"), "first result")),
            Message::tool_use(call("last", "record")),
            Message::tool_result(ToolResult::ok(&call("last", "record"), "x".repeat(4000))),
        ]
    }

    #[tokio::test]
    async fn official_compaction_summarizes_task_constraints_and_keeps_a_paired_tool_tail() {
        let scripted = Scripted::new(vec![
            round(vec![AgentEvent::TextDelta(
                "history summary: preserve parser flags".into(),
            )]),
            round(vec![AgentEvent::TextDelta("preserve parser flags".into())]),
        ]);
        let inner: Arc<dyn AgentBackend> = scripted.clone();
        let conversation = Conversation::new();
        conversation.replace_with(split_task_history());
        let policy = Compaction {
            enabled: true,
            reserve_tokens: 0,
            keep_recent_tokens: 1,
        };
        assert_eq!(
            compact_now(&inner, &conversation, policy, &Cancel::new()).await,
            Some(5)
        );
        let seen = scripted.requests();
        assert_eq!(seen.len(), 1);
        assert!(request_text(&seen[0]).contains("current target: parser.rs"));
        assert!(request_text(&seen[0]).contains("preserve flags"));
        let kept = conversation.snapshot();
        assert!(kept[0].text().contains("history summary"));
        assert!(
            rig_core::conversation::pairing::validate(conversation.native().unwrap().messages())
                .is_ok()
        );
        assert_eq!(
            kept[1].tool_calls().next().expect("retained call").id,
            "last"
        );
        assert_eq!(
            kept[2].tool_results().next().expect("retained result").id,
            "last"
        );
    }

    #[tokio::test]
    async fn a_failed_official_summary_does_not_change_saved_context() {
        let inner: Arc<dyn AgentBackend> =
            Scripted::new(vec![vec![AgentEvent::Error("summary failed".into())]]);
        let conversation = Conversation::new();
        conversation.replace_with(split_task_history());
        let before = conversation.snapshot();
        let policy = Compaction {
            enabled: true,
            reserve_tokens: 0,
            keep_recent_tokens: 1,
        };
        assert_eq!(
            compact_now(&inner, &conversation, policy, &Cancel::new()).await,
            None
        );
        assert_eq!(conversation.snapshot(), before);
    }
}
