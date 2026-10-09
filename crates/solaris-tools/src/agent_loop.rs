//! The loop that runs the model's tool calls and asks it again.
//!
//! A model cannot run anything: it can only ask. This wraps a real backend and
//! turns one request into as many model calls as the model needs — declaring the
//! tools, running what comes back, feeding the results in, and repeating until
//! the model stops asking. From the application's side it is just another
//! [`AgentBackend`], so nothing above it had to learn about tools.

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use futures::{Stream, StreamExt};
use solaris_backend::{AgentBackend, AgentEventStream};
use solaris_core::{
    AgentEvent, BackendError, Content, Message, Role, ToolCall, ToolResult, ToolSpec, TurnRequest,
    Usage,
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::approver::{AlwaysApprove, Approval, Approver};
use crate::registry::{ToolRegistry, ToolSelection};
use crate::tool::{Cancel, ExecutionMode, Tool, ToolContext};

/// Most tool round trips one turn may take before the loop gives up.
///
/// A model can get stuck asking for the same thing; without a ceiling the turn
/// would never end and never stop costing money.
pub const DEFAULT_MAX_ROUNDS: usize = 24;

/// Tool calls from one round that run at the same time.
///
/// Enough to overlap a round's reads without turning one model reply into a
/// burst of processes. A round containing a tool that must run alone ignores it.
pub const DEFAULT_MAX_PARALLEL: usize = 4;

/// What the loop needs to know about the session.
#[derive(Clone)]
pub struct ToolLoopOptions {
    /// Which tools the mode's default set is edited with.
    pub selection: ToolSelection,
    /// Who decides whether a call may run.
    pub approver: Arc<dyn Approver>,
    /// Directory relative paths resolve against.
    pub cwd: PathBuf,
    /// Where a truncated shell output is written.
    pub temp_dir: PathBuf,
    /// Most tool round trips per turn.
    pub max_rounds: usize,
    /// Most tool calls from one round that run at the same time.
    ///
    /// `1` runs every round one call at a time; a round holding a tool that
    /// declared itself sequential does that whatever this says.
    pub max_parallel: usize,
}

impl Default for ToolLoopOptions {
    fn default() -> Self {
        Self {
            selection: ToolSelection::none(),
            approver: Arc::new(AlwaysApprove),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            temp_dir: std::env::temp_dir(),
            max_rounds: DEFAULT_MAX_ROUNDS,
            max_parallel: DEFAULT_MAX_PARALLEL,
        }
    }
}

impl ToolLoopOptions {
    /// Options rooted at `cwd`, with the default temp directory.
    pub fn in_directory(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            ..Default::default()
        }
    }

    /// The same options with `approver` deciding about each call.
    pub fn with_approver(mut self, approver: Arc<dyn Approver>) -> Self {
        self.approver = approver;
        self
    }

    /// The same options with `selection` choosing the tools.
    pub fn with_selection(mut self, selection: ToolSelection) -> Self {
        self.selection = selection;
        self
    }
}

/// A backend that runs the model's tool calls.
pub struct ToolLoop {
    inner: Arc<dyn AgentBackend>,
    registry: Arc<ToolRegistry>,
    options: ToolLoopOptions,
    cancel: Cancel,
}

impl ToolLoop {
    /// A loop around `inner` that offers the registry's tools.
    pub fn new(
        inner: Arc<dyn AgentBackend>,
        registry: Arc<ToolRegistry>,
        options: ToolLoopOptions,
    ) -> Self {
        Self {
            inner,
            registry,
            options,
            cancel: Cancel::new(),
        }
    }

    /// A handle that stops a turn in flight: a running command is killed and no
    /// further model call is made.
    pub fn cancel_handle(&self) -> Cancel {
        self.cancel.clone()
    }

    /// The tools a request in `mode` would declare.
    pub fn tool_names(&self, mode: solaris_core::Mode) -> Vec<String> {
        self.registry.selected_names(mode, &self.options.selection)
    }
}

impl std::fmt::Debug for ToolLoop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolLoop")
            .field("inner", &self.inner.label())
            .field("tools", &self.registry.len())
            .field("max_rounds", &self.options.max_rounds)
            .finish()
    }
}

#[async_trait::async_trait]
impl AgentBackend for ToolLoop {
    async fn run_turn(&self, request: TurnRequest) -> Result<AgentEventStream, BackendError> {
        // A cancel belongs to one turn. Clearing it here is what keeps a stop
        // that arrived after the last turn from stopping this one.
        self.cancel.reset();

        let tools = self.registry.select(request.mode, &self.options.selection);
        let (tx, rx) = unbounded_channel();

        let runner = Runner {
            inner: Arc::clone(&self.inner),
            specs: tools.iter().map(|tool| tool.spec()).collect(),
            tools,
            approver: Arc::clone(&self.options.approver),
            cwd: self.options.cwd.clone(),
            temp_dir: self.options.temp_dir.clone(),
            cancel: self.cancel.clone(),
            max_rounds: self.options.max_rounds,
            max_parallel: self.options.max_parallel,
        };

        // The loop runs in its own task so the caller can keep drawing while a
        // command runs. Dropping the returned stream drops `tx`, which is how
        // the loop learns the turn was abandoned.
        tokio::spawn(async move {
            runner.drive(request, tx).await;
        });

        Ok(Box::pin(EventStream { rx }))
    }

    fn label(&self) -> &str {
        self.inner.label()
    }

    async fn models(&self) -> Result<Vec<String>, BackendError> {
        self.inner.models().await
    }
}

/// The state one turn's loop runs with, owned outright so it can outlive the
/// call to `run_turn`.
struct Runner {
    inner: Arc<dyn AgentBackend>,
    tools: Vec<Arc<dyn Tool>>,
    specs: Vec<ToolSpec>,
    approver: Arc<dyn Approver>,
    cwd: PathBuf,
    temp_dir: PathBuf,
    cancel: Cancel,
    max_rounds: usize,
    max_parallel: usize,
}

impl Runner {
    /// Ask the model, run what it asks for, and repeat until it stops asking.
    async fn drive(&self, request: TurnRequest, tx: UnboundedSender<AgentEvent>) {
        let mode = request.mode;
        let mut history = request.history.clone();
        let mut prompt = request.prompt.clone();
        let mut total = Usage::default();
        let mut cost_usd = 0.0;
        let mut rounds = 0usize;

        loop {
            if self.cancel.is_cancelled() {
                let _ = tx.send(AgentEvent::Error("the turn was cancelled".to_string()));
                return;
            }

            let round_request = TurnRequest {
                history: history.clone(),
                prompt: prompt.clone(),
                mode,
                tools: self.specs.clone(),
            };

            let stream = match self.inner.run_turn(round_request).await {
                Ok(stream) => stream,
                Err(error) => {
                    let _ = tx.send(AgentEvent::Error(error.to_string()));
                    return;
                }
            };

            let reply = match self.consume(stream, &tx, &mut total).await {
                RoundOutcome::Completed(reply) => reply,
                // The round ended in an error, which has already been sent.
                RoundOutcome::Failed => return,
            };

            cost_usd += reply.cost_usd;

            if reply.calls.is_empty() {
                let _ = tx.send(AgentEvent::TurnComplete {
                    usage: total,
                    cost_usd,
                });
                return;
            }

            rounds += 1;
            if rounds > self.max_rounds {
                let _ = tx.send(AgentEvent::Error(format!(
                    "stopped after {} rounds of tool calls — the model kept asking for more",
                    self.max_rounds
                )));
                return;
            }

            // The prompt belongs to the history from here on: later rounds
            // continue the same conversation rather than starting a new one.
            if !prompt.is_empty() {
                history.push(Message::user(prompt.as_str()));
                prompt.clear();
            }
            history.push(Message::with_content(
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
                return;
            };

            // Every result of one round rides in a single user message, which is
            // the shape the Messages API requires of a tool result.
            history.push(Message::with_content(Role::User, results));

            // A stop during the round ends the turn here: asking the model again
            // would only queue work for nobody.
            if self.cancel.is_cancelled() {
                let _ = tx.send(AgentEvent::Error("the turn was cancelled".to_string()));
                return;
            }
        }
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
    ) -> RoundOutcome {
        let mut text = String::new();
        let mut calls = Vec::new();
        let mut cost_usd = 0.0;
        let mut truncated = false;
        let mut completed = false;

        while let Some(event) = stream.next().await {
            match event {
                AgentEvent::TextDelta(chunk) => {
                    text.push_str(&chunk);
                    if tx.send(AgentEvent::TextDelta(chunk)).is_err() {
                        return RoundOutcome::Failed;
                    }
                }
                // The call is forwarded so a caller can show it while it runs;
                // the result follows once it is done.
                AgentEvent::ToolCall(call) => {
                    if tx.send(AgentEvent::ToolCall(call.clone())).is_err() {
                        return RoundOutcome::Failed;
                    }
                    calls.push(call);
                }
                // A cut-off message: remembered so the calls it carries are
                // refused rather than run, and passed on so the caller can say
                // so on screen.
                AgentEvent::OutputTruncated => {
                    truncated = true;
                    if tx.send(AgentEvent::OutputTruncated).is_err() {
                        return RoundOutcome::Failed;
                    }
                }
                AgentEvent::TurnComplete {
                    usage,
                    cost_usd: cost,
                } => {
                    *total = total.merge(usage);
                    cost_usd = cost;
                    completed = true;
                }
                AgentEvent::Error(message) => {
                    let _ = tx.send(AgentEvent::Error(message));
                    return RoundOutcome::Failed;
                }
                other => {
                    if tx.send(other).is_err() {
                        return RoundOutcome::Failed;
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
            return RoundOutcome::Failed;
        }

        RoundOutcome::Completed(RoundReply {
            text,
            calls,
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

/// How one round ended.
enum RoundOutcome {
    /// The model finished its message, which may ask for calls.
    Completed(RoundReply),
    /// The round failed. The error has already been sent to the caller.
    Failed,
}

/// What one model call produced.
struct RoundReply {
    /// The text the model wrote, which belongs in the history so later rounds
    /// see what it said around its calls.
    text: String,
    /// The calls it asked for.
    calls: Vec<ToolCall>,
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
    use solaris_core::{Mode, Usage};

    use crate::approver::DenyAll;
    use crate::registry::ToolRegistry;
    use crate::runner::CommandOutput;
    use crate::runner::tests::StubRunner;
    use crate::tool::ToolOutput;
    use crate::tools::shell::{ShellConfig, ShellOutcome, ShellRunner};

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

    /// A registry with one tool in it.
    fn registry_of(tool: Arc<dyn Tool>) -> ToolRegistry {
        ToolRegistry::with_tools(vec![tool])
    }

    /// A registry with one recording tool in it.
    fn registry_with(tool: Arc<Recording>) -> ToolRegistry {
        registry_of(tool as Arc<dyn Tool>)
    }

    /// Options that declare exactly `name`, since the mode's default set would
    /// name tools this registry does not have.
    fn only(name: &str) -> ToolLoopOptions {
        ToolLoopOptions::default().with_selection(ToolSelection {
            only: vec![name.to_string()],
            ..Default::default()
        })
    }

    /// Options that declare exactly the recording tool.
    fn record_options() -> ToolLoopOptions {
        only("record")
    }

    struct StubShell;

    #[async_trait]
    impl ShellRunner for StubShell {
        async fn run(
            &self,
            _config: ShellConfig,
            _command: &str,
            _ctx: &ToolContext,
            _timeout: Option<std::time::Duration>,
        ) -> Result<ShellOutcome, String> {
            Ok(ShellOutcome::default())
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

    /// The events a loop produces for one request.
    async fn drain(backend: Arc<dyn AgentBackend>, request: TurnRequest) -> Vec<AgentEvent> {
        let mut stream = backend.run_turn(request).await.expect("started");
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        events
    }

    fn loop_with(
        inner: Arc<dyn AgentBackend>,
        registry: ToolRegistry,
        options: ToolLoopOptions,
    ) -> Arc<ToolLoop> {
        Arc::new(ToolLoop::new(inner, Arc::new(registry), options))
    }

    fn request() -> TurnRequest {
        TurnRequest::new(
            vec![Message::system("You are solaris.")],
            "do it",
            Mode::Build,
        )
    }

    fn text(events: &[AgentEvent]) -> String {
        events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::TextDelta(chunk) => Some(chunk.as_str()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_turn_with_no_tool_call_ends_after_one_round() {
        let inner = Scripted::new(vec![round(vec![AgentEvent::TextDelta(
            "hello".to_string(),
        )])]);
        let tool = Arc::new(Recording::default());
        let backend = loop_with(
            inner.clone(),
            registry_with(Arc::clone(&tool)),
            record_options(),
        );

        let events = drain(backend, request()).await;

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
        let backend = loop_with(
            inner.clone(),
            registry_with(Arc::clone(&tool)),
            record_options(),
        );

        let events = drain(backend, request()).await;

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
        let results: Vec<&ToolResult> = second
            .history
            .iter()
            .flat_map(Message::tool_results)
            .collect();
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
        let backend = loop_with(
            inner,
            registry_with(Arc::new(Recording::default())),
            record_options(),
        );

        let events = drain(backend, request()).await;
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
        let backend = loop_with(
            inner.clone(),
            registry_with(Arc::new(Recording {
                calls: Mutex::new(Vec::new()),
                fail: true,
            })),
            record_options(),
        );

        let events = drain(backend, request()).await;

        let seen = inner.requests();
        let results: Vec<&ToolResult> = seen[1]
            .history
            .iter()
            .flat_map(Message::tool_results)
            .collect();
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
        let backend = loop_with(
            inner.clone(),
            registry_with(Arc::clone(&tool)),
            record_options().with_approver(Arc::new(DenyAll)),
        );

        drain(backend, request()).await;

        assert!(
            tool.calls.lock().expect("lock").is_empty(),
            "a declined call does not run"
        );
        let seen = inner.requests();
        let results: Vec<&ToolResult> = seen[1]
            .history
            .iter()
            .flat_map(Message::tool_results)
            .collect();
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
        let backend = loop_with(
            inner.clone(),
            registry_with(Arc::new(Recording::default())),
            record_options(),
        );

        drain(backend, request()).await;

        let seen = inner.requests();
        let results: Vec<&ToolResult> = seen[1]
            .history
            .iter()
            .flat_map(Message::tool_results)
            .collect();
        assert!(results[0].is_error);
        assert!(
            results[0].output.contains("no tool called"),
            "{}",
            results[0].output
        );
    }

    #[tokio::test]
    async fn the_loop_stops_a_model_that_never_stops_asking() {
        let rounds: Vec<Vec<AgentEvent>> = (0..10)
            .map(|index| {
                round(vec![AgentEvent::ToolCall(call(
                    &format!("call-{index}"),
                    "record",
                ))])
            })
            .collect();
        let inner = Scripted::new(rounds);
        let backend = loop_with(
            inner.clone(),
            registry_with(Arc::new(Recording::default())),
            ToolLoopOptions {
                max_rounds: 3,
                ..record_options()
            },
        );

        let events = drain(backend, request()).await;

        let Some(AgentEvent::Error(message)) = events.last() else {
            panic!("the loop should have given up: {events:?}");
        };
        assert!(message.contains("3 rounds"), "{message}");
        assert_eq!(
            inner.requests().len(),
            4,
            "three rounds plus the one that tripped it"
        );
    }

    #[tokio::test]
    async fn an_error_from_the_backend_ends_the_turn() {
        let inner = Scripted::new(vec![vec![AgentEvent::Error(
            "the model exploded".to_string(),
        )]]);
        let backend = loop_with(
            inner,
            registry_with(Arc::new(Recording::default())),
            record_options(),
        );

        let events = drain(backend, request()).await;
        assert_eq!(
            events,
            vec![AgentEvent::Error("the model exploded".to_string())]
        );
    }

    #[tokio::test]
    async fn the_backends_label_and_models_pass_through() {
        let inner = Scripted::new(vec![]);
        let backend = ToolLoop::new(
            inner,
            Arc::new(registry_with(Arc::new(Recording::default()))),
            record_options(),
        );

        assert_eq!(backend.label(), "scripted");
        assert!(
            backend.models().await.is_err(),
            "the scripted backend lists nothing"
        );
        assert_eq!(
            backend.tool_names(Mode::Build),
            vec!["record"],
            "the allowlist is the registry's own"
        );
    }

    #[tokio::test]
    async fn a_turn_asks_for_the_tools_its_mode_declares() {
        let inner = Scripted::new(vec![round(vec![])]);
        let backend = loop_with(
            inner.clone(),
            ToolRegistry::with_runners(
                Arc::new(StubRunner(CommandOutput::default())),
                Arc::new(StubShell),
            ),
            ToolLoopOptions::default(),
        );

        drain(backend, TurnRequest::new(Vec::new(), "hi", Mode::Plan)).await;

        let seen = inner.requests();
        let mut names: Vec<&str> = seen[0]
            .tools
            .iter()
            .map(|spec| spec.name.as_str())
            .collect();
        names.sort();
        assert_eq!(names, vec!["find", "grep", "ls", "read"]);
    }

    #[tokio::test]
    async fn dropping_the_stream_stops_the_loop() {
        /// A backend whose rounds all ask for another call, so the loop only
        /// ends when nothing is listening.
        struct Endless;

        #[async_trait]
        impl AgentBackend for Endless {
            async fn run_turn(
                &self,
                _request: TurnRequest,
            ) -> Result<AgentEventStream, BackendError> {
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
        let backend = loop_with(
            Arc::new(Endless),
            registry_with(Arc::clone(&tool)),
            record_options(),
        );

        let mut stream = backend.run_turn(request()).await.expect("started");
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

    #[tokio::test]
    async fn a_stream_that_ends_without_completing_is_an_error() {
        // No TurnComplete: the answer was cut short on the way.
        let inner = Scripted::new(vec![vec![AgentEvent::TextDelta("half".to_string())]]);
        let backend = loop_with(
            inner,
            registry_with(Arc::new(Recording::default())),
            record_options(),
        );

        let events = drain(backend, request()).await;

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
        let backend = loop_with(
            inner.clone(),
            registry_with(Arc::clone(&tool)),
            record_options(),
        );

        let events = drain(backend, request()).await;

        assert!(
            tool.calls.lock().expect("lock").is_empty(),
            "a half-written call must not run"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AgentEvent::OutputTruncated))
        );

        let seen = inner.requests();
        let results: Vec<&ToolResult> = seen[1]
            .history
            .iter()
            .flat_map(Message::tool_results)
            .collect();
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
    async fn calls_in_one_round_run_together_and_keep_their_order() {
        let calls: Vec<AgentEvent> = (0..4)
            .map(|index| AgentEvent::ToolCall(call(&format!("call-{index}"), "overlap")))
            .collect();
        let inner = Scripted::new(vec![
            round(calls),
            round(vec![AgentEvent::TextDelta("done".to_string())]),
        ]);
        let tool = Arc::new(Overlapping::default());
        let backend = loop_with(
            inner.clone(),
            registry_of(Arc::clone(&tool) as Arc<dyn Tool>),
            only("overlap"),
        );

        let events = drain(backend, request()).await;

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
        let seen = inner.requests();
        let results: Vec<&ToolResult> = seen[1]
            .history
            .iter()
            .flat_map(Message::tool_results)
            .collect();
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
        let backend = loop_with(
            inner,
            registry_of(Arc::clone(&tool) as Arc<dyn Tool>),
            ToolLoopOptions {
                max_parallel: 1,
                ..only("overlap")
            },
        );

        drain(backend, request()).await;

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
        let backend = loop_with(
            inner,
            registry_of(Arc::clone(&tool) as Arc<dyn Tool>),
            only("overlap"),
        );

        drain(backend, request()).await;

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
            round(vec![AgentEvent::ToolCall(call("call-3", "cancel-now"))]),
            round(vec![AgentEvent::TextDelta("after".to_string())]),
        ]);
        let tool = Arc::new(Cancelling::default());
        let backend = loop_with(
            inner,
            registry_of(Arc::clone(&tool) as Arc<dyn Tool>),
            only("cancel-now"),
        );
        // The first call cancels the turn it runs in.
        *tool.cancel.lock().expect("lock") = Some(backend.cancel_handle());

        let shared: Arc<dyn AgentBackend> = backend.clone();
        let cancelled = drain(shared, request()).await;

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
        let fresh: Arc<dyn AgentBackend> = backend;
        let next = drain(fresh, request()).await;
        assert!(
            matches!(next.last(), Some(AgentEvent::TurnComplete { .. })),
            "a fresh turn must not inherit the last turn's cancel: {next:?}"
        );
        assert_eq!(text(&next), "after");
    }

    #[tokio::test]
    async fn the_options_are_rooted_somewhere_sensible() {
        let options = ToolLoopOptions::default();
        assert!(options.cwd.is_absolute() || options.cwd == Path::new("."));
        assert!(options.max_parallel >= 1);
        assert_eq!(
            options.approver.approve(&call("call-1", "record")).await,
            Approval::Approved
        );
    }
}
