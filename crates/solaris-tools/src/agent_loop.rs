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
use crate::tool::{Cancel, Tool, ToolContext};

/// Most tool round trips one turn may take before the loop gives up.
///
/// A model can get stuck asking for the same thing; without a ceiling the turn
/// would never end and never stop costing money.
pub const DEFAULT_MAX_ROUNDS: usize = 24;

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
}

impl Default for ToolLoopOptions {
    fn default() -> Self {
        Self {
            selection: ToolSelection::none(),
            approver: Arc::new(AlwaysApprove),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            temp_dir: std::env::temp_dir(),
            max_rounds: DEFAULT_MAX_ROUNDS,
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
                Some(reply) => reply,
                // The round ended in an error, which has already been sent.
                None => return,
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

            let mut results = Vec::with_capacity(reply.calls.len());
            for call in &reply.calls {
                let started = Instant::now();
                let result = self.run_call(call).await;
                let elapsed = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
                if tx
                    .send(AgentEvent::ToolResult {
                        result: result.clone(),
                        duration_ms: elapsed,
                    })
                    .is_err()
                {
                    return;
                }
                results.push(Content::ToolResult { result });
            }

            // Every result of one round rides in a single user message, which is
            // the shape the Messages API requires of a tool result.
            history.push(Message::with_content(Role::User, results));
        }
    }

    /// Read one model call to its end, forwarding what it produces.
    ///
    /// `None` means the round failed and the error has been sent; `total` has
    /// this round's usage added to it either way.
    async fn consume(
        &self,
        mut stream: AgentEventStream,
        tx: &UnboundedSender<AgentEvent>,
        total: &mut Usage,
    ) -> Option<RoundReply> {
        let mut text = String::new();
        let mut calls = Vec::new();
        let mut cost_usd = 0.0;

        while let Some(event) = stream.next().await {
            match event {
                AgentEvent::TextDelta(chunk) => {
                    text.push_str(&chunk);
                    if tx.send(AgentEvent::TextDelta(chunk)).is_err() {
                        return None;
                    }
                }
                // The call is forwarded so a caller can show it while it runs;
                // the result follows once it is done.
                AgentEvent::ToolCall(call) => {
                    if tx.send(AgentEvent::ToolCall(call.clone())).is_err() {
                        return None;
                    }
                    calls.push(call);
                }
                AgentEvent::TurnComplete {
                    usage,
                    cost_usd: cost,
                } => {
                    *total = total.merge(usage);
                    cost_usd = cost;
                }
                AgentEvent::Error(message) => {
                    let _ = tx.send(AgentEvent::Error(message));
                    return None;
                }
                other => {
                    if tx.send(other).is_err() {
                        return None;
                    }
                }
            }
        }

        Some(RoundReply {
            text,
            calls,
            cost_usd,
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

        if self.approver.approve(call) == Approval::Denied {
            return ToolResult::error(
                call,
                "the call was declined, so it did not run — ask instead of acting, or try \
                 something else",
            );
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

    /// A registry with one recording tool in it.
    fn registry_with(tool: Arc<Recording>) -> ToolRegistry {
        ToolRegistry::with_tools(vec![tool as Arc<dyn Tool>])
    }

    /// Options that declare exactly the recording tool, since the mode's default
    /// set would name tools this registry does not have.
    fn record_options() -> ToolLoopOptions {
        ToolLoopOptions::default().with_selection(ToolSelection {
            only: vec!["record".to_string()],
            ..Default::default()
        })
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

    #[test]
    fn the_options_are_rooted_somewhere_sensible() {
        let options = ToolLoopOptions::default();
        assert!(options.cwd.is_absolute() || options.cwd == Path::new("."));
        assert!(options.approver.approve(&call("call-1", "record")) == Approval::Approved);
    }
}
