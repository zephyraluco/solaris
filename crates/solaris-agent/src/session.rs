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

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use futures::{Stream, StreamExt};
use solaris_backend::{
    AgentBackend, AgentEvent, AgentEventStream, Content, Message, Role, ToolCall, ToolResult,
    ToolSpec, TurnRequest, Usage,
};
use solaris_tools::{Cancel, ExecutionMode, Tool, ToolContext};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::agent::AgentOptions;
use crate::approver::{Approval, Approver};
use crate::compaction::Compaction;
use crate::conversation::Conversation;
use crate::overflow::{filled_the_window, is_overflow};
use crate::prompt::{
    COMPACT_PROMPT, UPDATE_COMPACT_PROMPT, conversation_text, existing_summary, summary_message,
};
use crate::queue::Queue;

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
}

/// Start a turn, streaming what it produces.
///
/// The loop runs in its own task so the caller can keep drawing while a command
/// runs. Dropping the returned stream drops `tx`, which is how the loop learns
/// the turn was abandoned.
pub(crate) fn start(input: TurnInput) -> AgentEventStream {
    // A cancel belongs to one turn. Clearing it here is what keeps a stop that
    // arrived after the last turn from stopping this one.
    input.cancel.reset();

    let TurnInput {
        inner,
        tools,
        options,
        cancel,
        steering,
        follow_up,
        conversation,
        system,
        prompt,
        context_window,
    } = input;

    let specs = tools
        .iter()
        .map(|tool| ToolSpec::new(tool.name(), tool.description(), tool.parameters()))
        .collect();
    let runner = Runner {
        inner,
        tools,
        specs,
        approver: Arc::clone(&options.approver),
        cwd: options.cwd.clone(),
        temp_dir: options.temp_dir.clone(),
        cancel,
        steering,
        follow_up,
        conversation,
        compaction: options.compaction,
        context_window,
        max_parallel: options.max_parallel,
        system,
    };

    let (tx, rx) = unbounded_channel();
    tokio::spawn(async move {
        runner.drive(prompt, tx).await;
    });

    Box::pin(EventStream { rx })
}

/// The state one turn's loop runs with, owned outright so it can outlive the
/// call that started it.
struct Runner {
    inner: Arc<dyn AgentBackend>,
    tools: Vec<Arc<dyn Tool>>,
    specs: Vec<ToolSpec>,
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
    max_parallel: usize,
    /// The system prompt, sent ahead of the conversation on every round.
    system: String,
}

impl Runner {
    /// Ask the model, run what it asks for, and repeat until it stops asking.
    ///
    /// There is no round ceiling: the model keeps going until it asks for
    /// nothing, fails, or the caller stops the turn. Cancelling is therefore the
    /// only bound on how much a turn may spend, which is why it is checked
    /// before every round and again after each round's calls have run.
    async fn drive(&self, prompt: String, tx: UnboundedSender<AgentEvent>) {
        // The prompt joins the conversation before anything is asked, so a turn
        // that is cancelled or fails still leaves what the user actually said.
        // It also settles the one ordering that matters here: anything steered in
        // afterwards is read *after* the words it answers.
        self.conversation.push(Message::user(prompt));

        let mut total = Usage::default();
        let mut cost_usd = 0.0;
        // What the provider reported for the last round, which is the honest
        // measure of how full the window is. An estimate is not.
        let mut last_usage: Option<Usage> = None;

        loop {
            if self.cancel.is_cancelled() {
                self.give_up("the turn was cancelled".to_string(), &tx);
                return;
            }

            self.absorb_steering(&tx);
            self.compact(last_usage.as_ref(), false, &tx).await;

            // One round, with one chance to recover from a window that turned out
            // to be full: the conversation is summarized and the same round asked
            // again, which is the difference between an error the user can do
            // nothing about and a turn that finishes.
            let mut recovered = false;
            let reply = loop {
                match self.ask(&tx, &mut total).await {
                    Round::Answered(reply) => break reply,
                    Round::Failed => {
                        self.conversation.repair();
                        return;
                    }
                    Round::Overflow(message) => {
                        if recovered || !self.compaction.enabled {
                            self.give_up(message, &tx);
                            return;
                        }
                        recovered = true;
                        if !self.compact(last_usage.as_ref(), true, &tx).await {
                            self.give_up(message, &tx);
                            return;
                        }
                    }
                }
            };

            last_usage = Some(reply.usage);
            cost_usd += reply.cost_usd;

            if reply.calls.is_empty() {
                // The answer belongs to the conversation as much as the question
                // does: without it the next turn would read every prompt the user
                // ever sent with nothing in between.
                self.record_reply(&reply.text);

                // A message that arrived while the model was answering is a
                // reason to keep going rather than to finish.
                if self.absorb_steering(&tx) {
                    continue;
                }
                if self.take_follow_ups(&tx) {
                    continue;
                }
                let _ = tx.send(AgentEvent::TurnComplete {
                    usage: total,
                    cost_usd,
                });
                return;
            }

            self.conversation.push(Message::with_content(
                Role::Assistant,
                assistant_blocks(reply.text, &reply.calls),
            ));

            // A message the model stopped writing early may carry half-written
            // calls, so none of them run: it is told to ask again with whole
            // arguments rather than have a guess acted on.
            let results = if reply.truncated {
                self.refuse_calls(&reply.calls, &tx)
            } else {
                self.run_round(&reply.calls, &tx).await
            };
            let Some(results) = results else {
                // The caller stopped listening mid-round; take back the request
                // whose answers never arrived so the conversation stays valid.
                self.conversation.repair();
                return;
            };
            self.conversation
                .push(Message::with_content(Role::User, results));

            // A stop during the round ends the turn here: asking the model again
            // would only queue work for nobody.
            if self.cancel.is_cancelled() {
                self.give_up("the turn was cancelled".to_string(), &tx);
                return;
            }
        }
    }

    /// Ask the model once, forwarding what it produces.
    async fn ask(&self, tx: &UnboundedSender<AgentEvent>, total: &mut Usage) -> Round {
        let request = TurnRequest {
            history: with_system(&self.system, &self.conversation.snapshot()),
            // The prompt is already in the conversation: this loop records it
            // before it asks anything.
            prompt: String::new(),
            tools: self.specs.clone(),
        };

        let stream = match self.inner.run_turn(request).await {
            Ok(stream) => stream,
            Err(error) => return self.report_failure(error.to_string(), tx),
        };
        self.consume(stream, tx, total).await
    }

    /// Decide what a failure means: an overflow can be recovered from, the rest
    /// cannot.
    fn report_failure(&self, message: String, tx: &UnboundedSender<AgentEvent>) -> Round {
        if is_overflow(&message) {
            Round::Overflow(message)
        } else {
            let _ = tx.send(AgentEvent::Error(message));
            Round::Failed
        }
    }

    /// End a turn without a completion, leaving the conversation consistent.
    fn give_up(&self, message: String, tx: &UnboundedSender<AgentEvent>) {
        self.conversation.repair();
        let _ = tx.send(AgentEvent::Error(message));
    }

    /// Record the model's own words, when it said anything.
    ///
    /// An empty message is left out rather than sent: some wires reject a blank
    /// text block outright, and a turn that answered nothing has nothing to add.
    fn record_reply(&self, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        self.conversation.push(Message::assistant(text));
    }

    /// Fold everything the user steered in into the conversation.
    ///
    /// Returns whether anything arrived, which is what tells the caller that a
    /// turn the model considered finished is not finished after all. Each message
    /// is reported as it is taken, so the caller that writes the transcript down
    /// learns that the model has now read it.
    fn absorb_steering(&self, tx: &UnboundedSender<AgentEvent>) -> bool {
        let steered = self.steering.drain();
        if steered.is_empty() {
            return false;
        }
        for text in steered {
            let _ = tx.send(AgentEvent::Steered { text: text.clone() });
            self.conversation.push(Message::user(text));
        }
        true
    }

    /// Take anything queued for after this turn, and say so.
    ///
    /// Returns whether the turn has more to do — a follow-up is a new turn inside
    /// the same loop, so the conversation carries on rather than being rebuilt.
    fn take_follow_ups(&self, tx: &UnboundedSender<AgentEvent>) -> bool {
        let queued = self.follow_up.drain();
        if queued.is_empty() {
            return false;
        }
        for prompt in queued {
            let _ = tx.send(AgentEvent::FollowUp {
                prompt: prompt.clone(),
            });
            self.conversation.push(Message::user(prompt));
        }
        true
    }

    /// Summarize the older part of the conversation, if there is a reason to.
    ///
    /// `force` is for the two cases where the budget is not the question: a round
    /// that came back with an overflow, and the user asking for it. Returns
    /// whether anything was replaced.
    async fn compact(
        &self,
        last_usage: Option<&Usage>,
        force: bool,
        tx: &UnboundedSender<AgentEvent>,
    ) -> bool {
        if !self.compaction.enabled {
            return false;
        }
        if !force && !self.over_budget(last_usage) {
            return false;
        }

        let messages = self.conversation.snapshot();
        let cut = self.compaction.cut_point(&messages);
        if cut == 0 {
            return false;
        }

        // A conversation that has been summarized before is updated rather than
        // summarized again, so what the first summary captured is not re-derived
        // from a conversation that no longer contains it.
        let previous = existing_summary(&messages[..cut]);
        let Some(summary) = summarize(
            &self.inner,
            &self.cancel,
            &messages[..cut],
            previous.as_deref(),
        )
        .await
        else {
            // Summarizing failed. Sending the conversation as it stands is worse
            // than asking for less, but far better than sending nothing.
            return false;
        };

        let mut replaced = Vec::with_capacity(messages.len() - cut + 1);
        replaced.push(summary_message(&summary));
        replaced.extend_from_slice(&messages[cut..]);
        self.conversation.replace_with(replaced);

        let _ = tx.send(AgentEvent::Compacted { dropped: cut });
        true
    }

    /// Whether the conversation has reached the point of needing a summary.
    fn over_budget(&self, last_usage: Option<&Usage>) -> bool {
        // A provider that accepted an over-long request and answered anyway will
        // never say so in an error, but its own usage says how much was read.
        if last_usage.is_some_and(|usage| filled_the_window(usage, self.context_window)) {
            return true;
        }
        let tokens = match last_usage {
            Some(usage) if !usage.estimated && !usage.is_empty() => usage.total(),
            _ => self.conversation.estimated_tokens(),
        };
        self.compaction.should_compact(tokens, self.context_window)
    }

    /// Read one round to its end, forwarding what it produces.
    ///
    /// A failed round has already had its error sent by the time `Failed`
    /// returns; `total` has this round's usage added to it either way.
    async fn consume(
        &self,
        mut stream: AgentEventStream,
        tx: &UnboundedSender<AgentEvent>,
        total: &mut Usage,
    ) -> Round {
        let mut text = String::new();
        let mut calls = Vec::new();
        let mut cost_usd = 0.0;
        let mut usage = Usage::default();
        let mut truncated = false;
        let mut completed = false;

        while let Some(event) = stream.next().await {
            match event {
                AgentEvent::TextDelta(chunk) => {
                    text.push_str(&chunk);
                    if tx.send(AgentEvent::TextDelta(chunk)).is_err() {
                        return Round::Failed;
                    }
                }
                // The call is forwarded so a caller can show it while it runs;
                // the result follows once it is done.
                AgentEvent::ToolCall(call) => {
                    if tx.send(AgentEvent::ToolCall(call.clone())).is_err() {
                        return Round::Failed;
                    }
                    calls.push(call);
                }
                // A cut-off message: remembered so the calls it carries are
                // refused rather than run, and passed on so the caller can say
                // so on screen.
                AgentEvent::OutputTruncated => {
                    truncated = true;
                    if tx.send(AgentEvent::OutputTruncated).is_err() {
                        return Round::Failed;
                    }
                }
                AgentEvent::TurnComplete {
                    usage: reported,
                    cost_usd: cost,
                } => {
                    *total = total.merge(reported);
                    usage = reported;
                    cost_usd = cost;
                    completed = true;
                }
                AgentEvent::Error(message) => return self.report_failure(message, tx),
                other => {
                    if tx.send(other).is_err() {
                        return Round::Failed;
                    }
                }
            }
        }

        // A stream that ends without a completion was cut short — the transport
        // dropped, a proxy closed the connection — and reading that as a
        // finished turn would report half an answer as the whole one.
        if !completed {
            let _ = tx.send(AgentEvent::Error(
                "the model's response ended without completing".to_string(),
            ));
            return Round::Failed;
        }

        Round::Answered(RoundReply {
            text,
            calls,
            usage,
            cost_usd,
            truncated,
        })
    }

    /// Run one call, or explain why it did not run.
    async fn run_call(&self, call: &ToolCall) -> ToolResult {
        let Some(tool) = self.tools.iter().find(|tool| tool.name() == call.name) else {
            return ToolResult::error(
                call,
                format!("there is no tool called `{}` in this session", call.name),
            );
        };

        if self.approver.approve(call).await == Approval::Denied {
            return ToolResult::error(
                call,
                "the call was declined, so it did not run — ask instead of acting, or try \
                 something else",
            );
        }

        // Waiting for that answer is exactly when a cancel is likely to arrive.
        if self.cancel.is_cancelled() {
            return ToolResult::error(call, "the turn was cancelled before this call ran");
        }

        let ctx = ToolContext {
            cwd: self.cwd.clone(),
            temp_dir: self.temp_dir.clone(),
            cancel: self.cancel.clone(),
        };
        let output = tool.run(call.input.clone(), &ctx).await;

        if output.is_error {
            ToolResult::error(call, output.text)
        } else {
            ToolResult::ok(call, output.text)
        }
    }

    /// Run one round's calls and answer with the blocks the next round carries.
    ///
    /// Calls that may run together do, up to the parallel cap; results come back
    /// in the order the model asked for them, which is what pairs each answer
    /// with its call. `None` means the caller stopped listening.
    async fn run_round(
        &self,
        calls: &[ToolCall],
        tx: &UnboundedSender<AgentEvent>,
    ) -> Option<Vec<Content>> {
        if self.runs_one_at_a_time(calls) {
            let mut results = Vec::with_capacity(calls.len());
            for call in calls {
                results.push(self.run_and_report(call, tx).await?);
            }
            return Some(results);
        }

        // Calls run in batches of the cap, so nothing waits on more than the
        // batch it is in and no closure has to name the futures' lifetimes.
        let mut results = Vec::with_capacity(calls.len());
        for batch in calls.chunks(self.max_parallel) {
            let mut pending = Vec::with_capacity(batch.len());
            for call in batch {
                pending.push(self.run_and_report(call, tx));
            }
            results.extend(futures::future::join_all(pending).await);
        }
        results.into_iter().collect()
    }

    /// Whether this round has to run one call at a time.
    ///
    /// True when the cap says so, or when any call in it is for a tool that
    /// asked to run alone: one such call settles the whole round, because the
    /// point of running alone is that nothing else overlaps it.
    fn runs_one_at_a_time(&self, calls: &[ToolCall]) -> bool {
        if self.max_parallel <= 1 {
            return true;
        }
        calls.iter().any(|call| {
            self.tools
                .iter()
                .find(|tool| tool.name() == call.name)
                .is_some_and(|tool| tool.execution_mode() == ExecutionMode::Sequential)
        })
    }

    /// Run one call and report it, answering with the block to send back.
    async fn run_and_report(
        &self,
        call: &ToolCall,
        tx: &UnboundedSender<AgentEvent>,
    ) -> Option<Content> {
        let started = Instant::now();
        // A stop that arrived between calls means nothing more should start.
        let result = if self.cancel.is_cancelled() {
            ToolResult::error(call, "the turn was cancelled before this call ran")
        } else {
            self.run_call(call).await
        };
        let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

        tx.send(AgentEvent::ToolResult {
            result: result.clone(),
            duration_ms: elapsed,
        })
        .ok()?;

        Some(Content::ToolResult { result })
    }

    /// Refuse every call in a message the model never finished writing.
    ///
    /// The arguments may be half a JSON object, so running them would mean
    /// acting on a guess. The model is told instead, and can ask again with the
    /// arguments written out in full.
    fn refuse_calls(
        &self,
        calls: &[ToolCall],
        tx: &UnboundedSender<AgentEvent>,
    ) -> Option<Vec<Content>> {
        let mut results = Vec::with_capacity(calls.len());
        for call in calls {
            let result = ToolResult::error(
                call,
                "the reply hit the output limit before this call was complete, so it did not \
                 run — ask again with the arguments written out in full",
            );
            tx.send(AgentEvent::ToolResult {
                result: result.clone(),
                duration_ms: 0,
            })
            .ok()?;
            results.push(Content::ToolResult { result });
        }
        Some(results)
    }
}

/// Summarize `older`, given the summary it replaces when there is one.
///
/// The summary call is the plainest request the loop can make: no tools, one
/// system message saying what is wanted, and an answer read here instead of being
/// forwarded — the caller asked for a turn's answer, not for the notes taken
/// along the way. It observes the turn's cancel, because a summary of a long
/// conversation is not something to sit through after the user has given up.
pub(crate) async fn summarize(
    inner: &Arc<dyn AgentBackend>,
    cancel: &Cancel,
    older: &[Message],
    previous: Option<&str>,
) -> Option<String> {
    let (system, prompt) = match previous {
        Some(existing) => (
            UPDATE_COMPACT_PROMPT,
            format!(
                "{}\n\n--- Summary to update ---\n{existing}",
                conversation_text(older)
            ),
        ),
        None => (COMPACT_PROMPT, conversation_text(older)),
    };

    let request = TurnRequest {
        history: vec![Message::system(system)],
        prompt,
        tools: Vec::new(),
    };
    let mut stream = inner.run_turn(request).await.ok()?;

    let mut summary = String::new();
    while let Some(event) = stream.next().await {
        if cancel.is_cancelled() {
            return None;
        }
        match event {
            AgentEvent::TextDelta(chunk) => summary.push_str(&chunk),
            AgentEvent::Error(_) => return None,
            // Thinking, status and anything else are not the summary.
            _ => {}
        }
    }
    (!summary.trim().is_empty()).then_some(summary)
}

/// Summarize the conversation now, at the user's request.
///
/// Not the loop's path: there is no turn to protect here, so nothing is forced
/// and nothing is retried, and a conversation with nothing worth summarizing
/// answers `None` rather than making a pointless call. Returns how many messages
/// the summary replaced.
pub(crate) async fn compact_now(
    inner: &Arc<dyn AgentBackend>,
    conversation: &Conversation,
    compaction: Compaction,
    cancel: &Cancel,
) -> Option<usize> {
    if !compaction.enabled {
        return None;
    }
    let messages = conversation.snapshot();
    let cut = compaction.cut_point(&messages);
    if cut == 0 {
        return None;
    }

    let previous = existing_summary(&messages[..cut]);
    let summary = summarize(inner, cancel, &messages[..cut], previous.as_deref()).await?;

    let mut replaced = Vec::with_capacity(messages.len() - cut + 1);
    replaced.push(summary_message(&summary));
    replaced.extend_from_slice(&messages[cut..]);
    conversation.replace_with(replaced);
    Some(cut)
}

/// The conversation with the system prompt ahead of it, as a request carries it.
fn with_system(system: &str, conversation: &[Message]) -> Vec<Message> {
    let mut messages = Vec::with_capacity(conversation.len() + 1);
    messages.push(Message::system(system));
    messages.extend_from_slice(conversation);
    messages
}

/// How one round ended.
enum Round {
    /// The model finished its message, which may ask for calls.
    Answered(RoundReply),
    /// The round failed. The error has already been sent to the caller.
    Failed,
    /// The window was full. The caller decides whether to summarize and retry.
    Overflow(String),
}

/// What one model call produced.
struct RoundReply {
    /// The text the model wrote, which belongs in the conversation so later
    /// rounds see what it said around its calls.
    text: String,
    /// The calls it asked for.
    calls: Vec<ToolCall>,
    /// What this round alone consumed, which is how full the window now is.
    usage: Usage,
    /// What the call cost.
    cost_usd: f64,
    /// Whether the output limit cut the message off.
    truncated: bool,
}

/// The assistant message for a round: its text, then its calls.
fn assistant_blocks(text: String, calls: &[ToolCall]) -> Vec<Content> {
    let mut blocks = Vec::with_capacity(calls.len() + 1);
    if !text.is_empty() {
        blocks.push(Content::text(text));
    }
    blocks.extend(calls.iter().cloned().map(|call| Content::ToolUse { call }));
    blocks
}

/// A stream over the loop's channel.
///
/// Hand-rolled rather than pulled from another crate: it is the only thing
/// needed from one, and it is three lines.
struct EventStream {
    rx: UnboundedReceiver<AgentEvent>,
}

impl Stream for EventStream {
    type Item = AgentEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<AgentEvent>> {
        self.rx.poll_recv(cx)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
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
                ToolOutput::ok("done")
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
            events,
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

        let results = results_seen(&inner);
        assert!(results[0].is_error);
        assert!(
            results[0].output.contains("output limit"),
            "{}",
            results[0].output
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::TurnComplete { .. })
        ));
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
    const SUMMARY_MARKER: &str = crate::prompt::SUMMARY_PREFIX;

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
        assert_eq!(seen.len(), 2, "a summary call, then the round");

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
            seen[0].prompt.contains("what came before"),
            "and is handed the summary it is updating: {}",
            seen[0].prompt
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::TurnComplete { .. })
        ));
    }

    #[tokio::test]
    async fn a_window_that_turns_out_to_be_full_is_recovered_from() {
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

        // The overflow did not become the turn's answer: it was summarized away
        // and the same round asked again.
        assert!(
            matches!(events.last(), Some(AgentEvent::TurnComplete { .. })),
            "the turn finished rather than failing: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AgentEvent::Compacted { .. })),
            "and it says what it did: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AgentEvent::Error(_))),
            "the overflow never reached the user: {events:?}"
        );
        assert_eq!(
            inner.requests().len(),
            3,
            "refused, summarized, asked again"
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
    async fn a_conversation_that_grows_past_the_window_is_summarized_mid_turn() {
        // The provider reports a usage that fills the window, which is the one
        // signal an over-long request that was accepted anyway leaves behind.
        let inner = Scripted::new(vec![
            vec![
                AgentEvent::ToolCall(call("call-1", "record")),
                AgentEvent::TurnComplete {
                    usage: Usage::new(190_000, 100),
                    cost_usd: 0.0,
                },
            ],
            round(vec![AgentEvent::TextDelta("a summary".to_string())]),
            round(vec![AgentEvent::TextDelta("done".to_string())]),
        ]);
        let options = AgentOptions {
            compaction: Compaction {
                enabled: true,
                reserve_tokens: 16_384,
                keep_recent_tokens: 30,
            },
            ..Default::default()
        };

        let events = Turn::of(inner.clone(), Arc::new(Recording::default()))
            .options(options)
            .context_window(200_000)
            .history(long_history(12))
            .run("do it")
            .await;

        assert!(
            events
                .iter()
                .any(|event| matches!(event, AgentEvent::Compacted { .. })),
            "the reported usage was enough to trigger it: {events:?}"
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::TurnComplete { .. })
        ));
    }
}
