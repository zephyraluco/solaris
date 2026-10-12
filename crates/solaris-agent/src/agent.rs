//! The session: a resolved backend, the tools a turn may offer, and the handles
//! the application holds on to.

use std::path::PathBuf;
use std::sync::Arc;

use solaris_backend::{AgentEventStream, BackendError, Message};
use solaris_provider::{BackendChoice, CredentialSource};
use solaris_tools::{Cancel, ToolRegistry, ToolSelection};

use crate::approver::{AlwaysApprove, Approver};
use crate::mode::{Mode, default_tool_names};
use crate::session::{self, DEFAULT_MAX_PARALLEL};

/// One turn, as the application asks for it.
#[derive(Debug, Clone)]
pub struct AgentRequest {
    /// Prior conversation, oldest first, excluding `prompt`.
    pub history: Vec<Message>,
    /// The new user prompt.
    pub prompt: String,
    /// The mode the turn runs in, which decides the tools it may use.
    pub mode: Mode,
}

impl AgentRequest {
    /// A request in `mode`, with nothing said yet.
    pub fn new(history: Vec<Message>, prompt: impl Into<String>, mode: Mode) -> Self {
        Self {
            history,
            prompt: prompt.into(),
            mode,
        }
    }
}

/// What a session is told once, and keeps for every turn of it.
#[derive(Clone)]
pub struct AgentOptions {
    /// How the mode's default tool set is edited.
    pub selection: ToolSelection,
    /// Who decides whether a call may run.
    pub approver: Arc<dyn Approver>,
    /// Directory relative tool paths resolve against.
    pub cwd: PathBuf,
    /// Where a truncated shell output is written.
    pub temp_dir: PathBuf,
    /// Most tool calls from one round that run at the same time.
    ///
    /// `1` runs every round one call at a time; a round holding a tool that
    /// declared itself sequential does that whatever this says.
    ///
    /// There is no cap on how many rounds a turn may take: the loop runs until
    /// the model stops asking, fails, or the turn is cancelled.
    pub max_parallel: usize,
}

impl Default for AgentOptions {
    fn default() -> Self {
        Self {
            selection: ToolSelection::none(),
            approver: Arc::new(AlwaysApprove),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            temp_dir: std::env::temp_dir(),
            max_parallel: DEFAULT_MAX_PARALLEL,
        }
    }
}

impl AgentOptions {
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

    /// The same options with `selection` editing the mode's default tool set.
    pub fn with_selection(mut self, selection: ToolSelection) -> Self {
        self.selection = selection;
        self
    }
}

/// A session with a model behind it.
///
/// The application holds one of these and asks it for turns; everything else —
/// which tools a turn declares, running them, feeding the results back, asking
/// again — happens behind this type. So the app never learns which tools exist,
/// how many model calls a turn took, or which platform answered.
pub struct Agent {
    /// The backend the credentials resolved to, and how it was chosen.
    choice: BackendChoice,
    /// Every tool this platform has, built once so `edit` and `write` keep
    /// serialising against the same per-file locks for the whole session.
    registry: Arc<ToolRegistry>,
    options: AgentOptions,
    /// Stops the turn in flight, tools included.
    cancel: Cancel,
}

impl Agent {
    /// A session over `choice`, offering the registry's tools.
    pub fn new(choice: BackendChoice, registry: Arc<ToolRegistry>, options: AgentOptions) -> Self {
        Self {
            choice,
            registry,
            options,
            cancel: Cancel::new(),
        }
    }

    /// A handle that stops a turn in flight: a running command is killed and no
    /// further model call is made.
    ///
    /// Every turn of this session shares one, so a stop reaches whichever turn
    /// is running.
    pub fn cancel_handle(&self) -> Cancel {
        self.cancel.clone()
    }

    /// Short human-readable label shown in the status bar.
    pub fn label(&self) -> &str {
        self.choice.backend.label()
    }

    /// The provider requests go to, when one was picked at all.
    pub fn provider_id(&self) -> Option<&'static str> {
        self.choice.provider_id
    }

    /// The model that will really be asked for, which is not always the one that
    /// was requested: an unnamed model is filled in with the provider's first
    /// offer.
    pub fn model(&self) -> &str {
        &self.choice.model
    }

    /// Where the credential behind this session came from.
    pub fn source(&self) -> CredentialSource {
        self.choice.source
    }

    /// The models the endpoint offers, for the picker to choose from.
    pub async fn models(&self) -> Result<Vec<String>, BackendError> {
        self.choice.backend.models().await
    }

    /// The tools a turn in `mode` would declare.
    pub fn tool_names(&self, mode: Mode) -> Vec<String> {
        self.registry
            .selected_names(default_tool_names(mode), &self.options.selection)
    }

    /// Run one turn, streaming what it produces.
    ///
    /// The turn runs in its own task, so the caller can keep drawing while a
    /// command runs; dropping the stream abandons it. A failure to answer is an
    /// [`AgentEvent::Error`](crate::AgentEvent::Error) in the stream rather than
    /// a start-up error, because the loop is what finds out.
    pub fn run_turn(&self, request: AgentRequest) -> AgentEventStream {
        let tools = self
            .registry
            .select(default_tool_names(request.mode), &self.options.selection);

        session::start(
            Arc::clone(&self.choice.backend),
            tools,
            &self.options,
            self.cancel.clone(),
            request.history,
            request.prompt,
        )
    }
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agent")
            .field("backend", &self.label())
            .field("tools", &self.registry.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use futures::StreamExt;
    use serde_json::{Value, json};
    use solaris_backend::{
        AgentBackend, AgentEvent, AgentEventStream, BackendError, ToolCall, TurnRequest, Usage,
    };
    use solaris_tools::{SHELL_TOOL, Tool, ToolContext, ToolOutput};

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

    /// A backend that lists one model, for the pass-through checks.
    struct Listed;

    #[async_trait]
    impl AgentBackend for Listed {
        async fn run_turn(&self, _request: TurnRequest) -> Result<AgentEventStream, BackendError> {
            Ok(Box::pin(futures::stream::iter(Vec::new())))
        }

        fn label(&self) -> &str {
            "listed"
        }

        async fn models(&self) -> Result<Vec<String>, BackendError> {
            Ok(vec!["first".to_string()])
        }
    }

    /// A tool that stops the turn it is running in.
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
            if let Some(cancel) = self.cancel.lock().expect("lock").take() {
                cancel.cancel();
            }
            ToolOutput::ok("done")
        }
    }

    /// A backend as the choice carries it, with a provider behind it.
    fn choice(backend: Arc<dyn AgentBackend>) -> BackendChoice {
        BackendChoice {
            backend,
            provider_id: Some("anthropic"),
            base_url: Some("https://api.anthropic.com/v1".to_string()),
            model: "claude-sonnet-4-5".to_string(),
            source: CredentialSource::Stored,
        }
    }

    /// A session over `backend`, offering every built-in tool.
    fn agent(backend: Arc<dyn AgentBackend>) -> Agent {
        Agent::new(
            choice(backend),
            Arc::new(ToolRegistry::builtin()),
            AgentOptions::default(),
        )
    }

    /// A tool as the registry and the loop want it.
    fn as_tool<T: Tool + 'static>(tool: Arc<T>) -> Arc<dyn Tool> {
        tool
    }

    /// Options that declare exactly `name` — the registry in a test holds tools
    /// the mode's default set does not name.
    fn only(name: &str) -> AgentOptions {
        AgentOptions::default().with_selection(ToolSelection {
            only: vec![name.to_string()],
            ..Default::default()
        })
    }

    /// Read a turn's stream to its end.
    async fn drain(stream: AgentEventStream) -> Vec<AgentEvent> {
        let mut stream = stream;
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        events
    }

    #[test]
    fn build_mode_offers_more_than_plan_mode() {
        let agent = agent(Scripted::new(Vec::new()));

        let build = agent.tool_names(Mode::Build);
        assert!(build.contains(&"read".to_string()));
        assert!(build.contains(&SHELL_TOOL.to_string()));
        assert!(build.contains(&"edit".to_string()));
        assert!(build.contains(&"write".to_string()));

        assert_eq!(agent.tool_names(Mode::Plan), ["find", "grep", "ls", "read"]);
    }

    #[test]
    fn an_allowlist_overrides_what_the_mode_offers() {
        let agent = Agent::new(
            choice(Scripted::new(Vec::new())),
            Arc::new(ToolRegistry::builtin()),
            only("read"),
        );

        assert_eq!(agent.tool_names(Mode::Build), ["read"]);
        assert_eq!(agent.tool_names(Mode::Plan), ["read"]);
    }

    #[tokio::test]
    async fn a_turn_declares_the_tools_its_mode_offers() {
        let inner = Scripted::new(vec![Vec::new()]);
        let agent = agent(inner.clone());

        drain(agent.run_turn(AgentRequest::new(Vec::new(), "hi", Mode::Plan))).await;

        let mut names: Vec<String> = inner.requests()[0]
            .tools
            .iter()
            .map(|spec| spec.name.clone())
            .collect();
        names.sort();
        assert_eq!(names, ["find", "grep", "ls", "read"]);
    }

    #[tokio::test]
    async fn a_turn_declares_what_the_tool_describes_itself_as() {
        let inner = Scripted::new(vec![Vec::new()]);
        let tool = Arc::new(Cancelling::default());
        let agent = Agent::new(
            choice(inner.clone()),
            Arc::new(ToolRegistry::with_tools(vec![as_tool(Arc::clone(&tool))])),
            only("cancel-now"),
        );

        drain(agent.run_turn(AgentRequest::new(Vec::new(), "hi", Mode::Build))).await;

        let spec = &inner.requests()[0].tools[0];
        assert_eq!(spec.name, tool.name());
        assert_eq!(spec.description, tool.description());
        assert_eq!(spec.parameters, tool.parameters());
    }

    #[tokio::test]
    async fn the_backends_label_and_models_pass_through() {
        let agent = agent(Arc::new(Listed));

        assert_eq!(agent.label(), "listed");
        assert_eq!(agent.models().await.expect("a list"), ["first"]);
    }

    #[test]
    fn the_session_reports_the_choice_it_was_built_from() {
        let agent = agent(Scripted::new(Vec::new()));

        assert_eq!(agent.provider_id(), Some("anthropic"));
        assert_eq!(agent.model(), "claude-sonnet-4-5");
        assert_eq!(agent.source(), CredentialSource::Stored);
    }

    #[test]
    fn a_session_nothing_was_connected_for_says_so() {
        let mut choice = choice(Scripted::new(Vec::new()));
        choice.provider_id = None;
        choice.model = String::new();
        choice.source = CredentialSource::Missing;

        let agent = Agent::new(
            choice,
            Arc::new(ToolRegistry::builtin()),
            AgentOptions::default(),
        );
        assert_eq!(agent.provider_id(), None);
        assert_eq!(agent.model(), "");
        assert_eq!(agent.source(), CredentialSource::Missing);
    }

    #[tokio::test]
    async fn the_handle_a_session_hands_out_stops_the_turn_it_is_running() {
        let tool = Arc::new(Cancelling::default());
        let inner = Scripted::new(vec![vec![
            AgentEvent::ToolCall(ToolCall::new("call-1", "cancel-now", json!({}))),
            AgentEvent::TurnComplete {
                usage: Usage::new(1, 1),
                cost_usd: 0.0,
            },
        ]]);
        let agent = Agent::new(
            choice(inner),
            Arc::new(ToolRegistry::with_tools(vec![as_tool(Arc::clone(&tool))])),
            only("cancel-now"),
        );

        // The tool stops the turn through the handle the application holds, so
        // the stop has to reach the loop that is running.
        *tool.cancel.lock().expect("lock") = Some(agent.cancel_handle());

        let events = drain(agent.run_turn(AgentRequest::new(Vec::new(), "hi", Mode::Build))).await;

        assert_eq!(tool.runs.load(Ordering::SeqCst), 1);
        let Some(AgentEvent::Error(message)) = events.last() else {
            panic!("a cancelled turn ends in an error: {events:?}");
        };
        assert!(message.contains("cancelled"), "{message}");
    }
}
