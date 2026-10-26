//! Delegation through the public Agent API, with scripted model replies.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{Value, json};
use solaris_agent::{
    Agent, AgentBackend, AgentEvent, AgentEventStream, AgentOptions, Approval, BackendError,
    FnApprover, Mode, PromptContext, PromptFile, Role, Tool, ToolCall, ToolContext, ToolOutput,
    ToolRegistry, ToolSelection, TurnRequest, Usage,
};
use solaris_provider::{BackendChoice, CredentialSource};
use tokio::sync::{Barrier, Notify};

fn complete(text: &str) -> Vec<AgentEvent> {
    vec![
        AgentEvent::TextDelta(text.into()),
        AgentEvent::TurnComplete {
            usage: Usage::new(2, 1),
            cost_usd: 0.25,
        },
    ]
}

fn calling(name: &str, input: Value) -> Vec<AgentEvent> {
    vec![
        AgentEvent::ToolCall(ToolCall::new("work", name, input)),
        AgentEvent::TurnComplete {
            usage: Usage::new(2, 1),
            cost_usd: 0.25,
        },
    ]
}

struct Model {
    seen: Mutex<Vec<TurnRequest>>,
    kind: &'static str,
    model_override: Option<String>,
    resume_id: Option<String>,
    child_tool: Option<&'static str>,
    fail: bool,
    endless: bool,
    parallel: bool,
    barrier: Barrier,
    started: Notify,
    hang: bool,
    active: AtomicUsize,
}

impl Model {
    fn new(kind: &'static str) -> Self {
        Self {
            seen: Mutex::default(),
            kind,
            model_override: None,
            resume_id: None,
            child_tool: None,
            fail: false,
            endless: false,
            parallel: false,
            barrier: Barrier::new(2),
            started: Notify::new(),
            hang: false,
            active: AtomicUsize::new(0),
        }
    }
    fn requests(&self) -> Vec<TurnRequest> {
        self.seen.lock().unwrap().clone()
    }
}

fn is_child(request: &TurnRequest) -> bool {
    request.history.iter().any(|message| {
        message.role == Role::User && message.text().starts_with("You are an isolated")
    })
}

#[async_trait]
impl AgentBackend for Model {
    async fn run_turn(&self, request: TurnRequest) -> Result<AgentEventStream, BackendError> {
        let child = is_child(&request);
        let answered = request
            .history
            .iter()
            .any(|message| message.tool_results().next().is_some());
        self.seen.lock().unwrap().push(request);
        let events = if child {
            if self.hang {
                struct Active<'a>(&'a AtomicUsize);
                impl Drop for Active<'_> {
                    fn drop(&mut self) {
                        self.0.fetch_sub(1, Ordering::SeqCst);
                    }
                }
                self.active.fetch_add(1, Ordering::SeqCst);
                let _guard = Active(&self.active);
                self.started.notify_one();
                std::future::pending::<()>().await;
            }
            if self.parallel {
                self.barrier.wait().await;
            }
            if self.fail {
                return Err(BackendError::new("child backend unavailable"));
            }
            if let Some(tool) = self.child_tool.filter(|_| !answered || self.endless) {
                calling(tool, json!({"path":"child-path"}))
            } else {
                complete("child final report")
            }
        } else if answered {
            complete("parent final reply")
        } else if self.parallel {
            vec![
                AgentEvent::ToolCall(ToolCall::new(
                    "one",
                    "subagent",
                    json!({"agent":self.kind,"prompt":"Investigate A"}),
                )),
                AgentEvent::ToolCall(ToolCall::new(
                    "two",
                    "subagent",
                    json!({"agent":self.kind,"prompt":"Investigate B"}),
                )),
                AgentEvent::TurnComplete {
                    usage: Usage::new(2, 1),
                    cost_usd: 0.25,
                },
            ]
        } else {
            let mut input = json!({"agent":self.kind,"prompt":"Investigate the delegated task"});
            if let Some(model) = &self.model_override {
                input["model"] = json!(model);
            }
            if let Some(id) = &self.resume_id {
                input["resume_id"] = json!(id);
            }
            calling("subagent", input)
        };
        Ok(Box::pin(futures::stream::iter(events)))
    }
    fn label(&self) -> &str {
        "scripted"
    }
}

struct RecordingTool {
    name: &'static str,
    runs: Arc<AtomicUsize>,
}
#[async_trait]
impl Tool for RecordingTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "Record a tool invocation"
    }
    fn parameters(&self) -> Value {
        json!({"type":"object"})
    }
    async fn run(&self, _input: Value, _ctx: &ToolContext) -> ToolOutput {
        self.runs.fetch_add(1, Ordering::SeqCst);
        ToolOutput::ok("private child tool output")
    }
}

fn agent(model: Arc<Model>, options: AgentOptions, runs: &Arc<AtomicUsize>) -> Agent {
    let registry = ToolRegistry::with_tools(vec![
        Arc::new(RecordingTool {
            name: "read",
            runs: Arc::clone(runs),
        }),
        Arc::new(RecordingTool {
            name: "write",
            runs: Arc::clone(runs),
        }),
        Arc::new(RecordingTool {
            name: "grep",
            runs: Arc::clone(runs),
        }),
    ]);
    Agent::new(
        BackendChoice {
            backend: model,
            provider_id: None,
            base_url: None,
            model: "test-model".into(),
            source: CredentialSource::None,
        },
        Arc::new(registry),
        options,
    )
}

async fn run(agent: &Agent, mode: Mode) -> Vec<AgentEvent> {
    tokio::time::timeout(
        Duration::from_secs(3),
        agent.run_turn("ROOT_PRIVATE_CONTEXT", mode).collect(),
    )
    .await
    .expect("turn must finish")
}

fn result(events: &[AgentEvent]) -> &solaris_agent::ToolResult {
    events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ToolResult { result, .. } => Some(result),
            _ => None,
        })
        .expect("delegated tool result")
}

#[tokio::test]
async fn child_has_independent_context_and_only_returns_its_final_report() {
    let mut model = Model::new("explore");
    model.child_tool = Some("read");
    let model = Arc::new(model);
    let runs = Arc::new(AtomicUsize::new(0));
    let options = AgentOptions::default().with_prompt_context(PromptContext {
        project_rules: vec![PromptFile {
            path: "AGENTS.md".into(),
            content: "PROJECT_RULE".into(),
        }],
        ..Default::default()
    });
    let agent = agent(Arc::clone(&model), options, &runs);
    let events = run(&agent, Mode::Build).await;
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    assert_eq!(result(&events).output, "child final report");
    let text: String = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::TextDelta(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "parent final reply");
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::ToolCall(_)))
            .count(),
        1
    );
    for request in model.requests().iter().filter(|request| is_child(request)) {
        assert!(
            !request
                .history
                .iter()
                .any(|message| message.text().contains("ROOT_PRIVATE_CONTEXT"))
        );
        assert!(request.history[0].text().contains("PROJECT_RULE"));
        assert!(
            request
                .tools
                .iter()
                .all(|tool| tool.name != "subagent" && tool.name != "write")
        );
    }
    assert_eq!(agent.conversation_len(), 4);
    assert!(events.iter().any(|event| matches!(event,
        AgentEvent::SubagentStarted { call_id, task_id }
            if call_id == "work" && task_id == "foreground-work")));
    assert!(events.iter().any(|event| matches!(event,
        AgentEvent::SubagentToolCall { task_id, call }
            if task_id == "foreground-work" && call.name == "read")));
    assert!(events.iter().any(|event| matches!(event,
        AgentEvent::SubagentToolResult { task_id, result, .. }
            if task_id == "foreground-work" && result.output == "private child tool output")));
    for request in model.requests().iter().filter(|request| !is_child(request)) {
        assert!(!request.history.iter().any(|message| {
            message.text().contains("private child tool output")
                || message
                    .tool_results()
                    .any(|result| result.output.contains("private child tool output"))
        }));
    }
    assert!(
        matches!(events.last(), Some(AgentEvent::TurnComplete {usage, cost_usd})
        if *usage == Usage::new(8, 4) && *cost_usd == 1.0)
    );
}

#[tokio::test]
async fn general_child_executes_tools_and_obeys_inherited_approval() {
    for allowed in [true, false] {
        let mut model = Model::new("general");
        model.child_tool = Some("write");
        let runs = Arc::new(AtomicUsize::new(0));
        let options = AgentOptions::default().with_approver(Arc::new(FnApprover::new(
            move |call: &ToolCall| {
                if call.name == "write" && !allowed {
                    Approval::Denied
                } else {
                    Approval::Approved
                }
            },
        )));
        let agent = agent(Arc::new(model), options, &runs);
        let events = run(&agent, Mode::Build).await;
        assert_eq!(runs.load(Ordering::SeqCst), usize::from(allowed));
        assert!(matches!(
            events.last(),
            Some(AgentEvent::TurnComplete { .. })
        ));
    }
}

#[tokio::test]
async fn explore_cannot_gain_write_access_through_an_explicit_allowlist() {
    let mut model = Model::new("explore");
    model.child_tool = Some("write");
    let model = Arc::new(model);
    let runs = Arc::new(AtomicUsize::new(0));
    let options = AgentOptions::default().with_selection(ToolSelection {
        only: vec!["subagent".into(), "write".into()],
        ..Default::default()
    });
    let agent = agent(Arc::clone(&model), options, &runs);
    run(&agent, Mode::Build).await;
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    let requests = model.requests();
    let child = requests.iter().find(|request| is_child(request)).unwrap();
    assert!(child.tools.is_empty());
}

#[tokio::test]
async fn a_general_child_preserves_explicitly_selected_tools_outside_build_defaults() {
    let mut model = Model::new("general");
    model.child_tool = Some("grep");
    let model = Arc::new(model);
    let runs = Arc::new(AtomicUsize::new(0));
    let agent = agent(
        Arc::clone(&model),
        AgentOptions::default().with_selection(ToolSelection {
            only: vec!["subagent".into(), "grep".into()],
            ..Default::default()
        }),
        &runs,
    );
    run(&agent, Mode::Build).await;
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    let requests = model.requests();
    let child = requests.iter().find(|request| is_child(request)).unwrap();
    assert_eq!(
        child
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        vec!["grep"]
    );
}

#[tokio::test]
async fn plan_refuses_general_delegation_without_starting_a_child() {
    let model = Arc::new(Model::new("general"));
    let agent = agent(
        Arc::clone(&model),
        AgentOptions::default(),
        &Arc::new(AtomicUsize::new(0)),
    );
    let events = run(&agent, Mode::Plan).await;
    assert!(result(&events).is_error);
    assert!(result(&events).output.contains("Plan mode"));
    assert!(!model.requests().iter().any(is_child));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnComplete { .. })
    ));
}

#[tokio::test]
async fn plan_can_delegate_read_only_exploration() {
    let mut model = Model::new("explore");
    model.child_tool = Some("read");
    let runs = Arc::new(AtomicUsize::new(0));
    let agent = agent(Arc::new(model), AgentOptions::default(), &runs);
    let events = run(&agent, Mode::Plan).await;
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    assert!(!result(&events).is_error);
}

#[tokio::test]
async fn children_cannot_recursively_delegate_even_if_the_model_asks() {
    let mut model = Model::new("general");
    model.child_tool = Some("subagent");
    let model = Arc::new(model);
    let agent = agent(
        Arc::clone(&model),
        AgentOptions::default(),
        &Arc::new(AtomicUsize::new(0)),
    );
    run(&agent, Mode::Build).await;
    let requests = model.requests();
    assert_eq!(
        requests.iter().filter(|request| is_child(request)).count(),
        2
    );
    let response = requests
        .iter()
        .filter(|request| is_child(request))
        .nth(1)
        .unwrap();
    assert!(
        response
            .history
            .iter()
            .flat_map(|message| message.tool_results())
            .any(|result| result.is_error && result.output.contains("no tool called `subagent`"))
    );
}

#[tokio::test]
async fn child_failure_is_a_tool_error_not_a_parent_terminal_event() {
    let mut model = Model::new("explore");
    model.fail = true;
    let agent = agent(
        Arc::new(model),
        AgentOptions::default(),
        &Arc::new(AtomicUsize::new(0)),
    );
    let events = run(&agent, Mode::Build).await;
    assert!(result(&events).is_error);
    assert!(result(&events).output.contains("child backend unavailable"));
    assert_eq!(events.iter().filter(|event| event.is_terminal()).count(), 1);
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnComplete { .. })
    ));
}

#[tokio::test]
async fn child_budget_stops_an_endless_tool_loop_and_counts_consumed_usage() {
    let mut model = Model::new("explore");
    model.child_tool = Some("read");
    model.endless = true;
    let model = Arc::new(model);
    let runs = Arc::new(AtomicUsize::new(0));
    let agent = agent(Arc::clone(&model), AgentOptions::default(), &runs);
    let events = run(&agent, Mode::Build).await;
    assert!(result(&events).is_error);
    assert!(result(&events).output.contains("20 model-request budget"));
    assert_eq!(runs.load(Ordering::SeqCst), 20);
    assert_eq!(
        model
            .requests()
            .iter()
            .filter(|request| is_child(request))
            .count(),
        20
    );
    assert!(
        matches!(events.last(), Some(AgentEvent::TurnComplete {usage, cost_usd})
        if *usage == Usage::new(44, 22) && *cost_usd == 5.5)
    );
}

#[tokio::test]
async fn independent_subagents_run_concurrently_and_keep_result_order() {
    let mut model = Model::new("explore");
    model.parallel = true;
    let model = Arc::new(model);
    let agent = agent(
        Arc::clone(&model),
        AgentOptions::default(),
        &Arc::new(AtomicUsize::new(0)),
    );
    let events = run(&agent, Mode::Build).await;
    let results: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolResult { result, .. } => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|result| !result.is_error));
    let requests = model.requests();
    assert_eq!(
        requests.iter().filter(|request| is_child(request)).count(),
        2
    );
    let parent = requests.last().unwrap();
    let ids: Vec<_> = parent
        .history
        .iter()
        .flat_map(|message| message.tool_results())
        .map(|result| result.id.as_str())
        .collect();
    assert_eq!(ids, vec!["one", "two"]);
}

#[tokio::test]
async fn parallel_child_tools_with_identical_ids_keep_distinct_display_task_ids() {
    let mut model = Model::new("explore");
    model.parallel = true;
    model.child_tool = Some("read");
    let agent = agent(
        Arc::new(model),
        AgentOptions::default(),
        &Arc::new(AtomicUsize::new(0)),
    );
    let events = run(&agent, Mode::Build).await;
    let mut scopes: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::SubagentToolCall { task_id, call } => {
                assert_eq!(call.id, "work");
                Some(task_id.as_str())
            }
            _ => None,
        })
        .collect();
    scopes.sort_unstable();
    assert_eq!(scopes, ["foreground-one", "foreground-two"]);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::SubagentToolResult { .. }))
            .count(),
        2
    );
}

#[tokio::test]
async fn cancellation_and_dropped_parent_stream_stop_pending_child_network_requests() {
    for drop_stream in [false, true] {
        let mut model = Model::new("explore");
        model.hang = true;
        let model = Arc::new(model);
        let agent = agent(
            Arc::clone(&model),
            AgentOptions::default(),
            &Arc::new(AtomicUsize::new(0)),
        );
        let mut stream = agent.run_turn("ROOT_PRIVATE_CONTEXT", Mode::Build);
        tokio::time::timeout(Duration::from_secs(1), model.started.notified())
            .await
            .unwrap();
        assert_eq!(model.active.load(Ordering::SeqCst), 1);
        if drop_stream {
            drop(stream);
        } else {
            agent.cancel_handle().cancel();
            tokio::time::timeout(Duration::from_secs(1), async {
                while let Some(event) = stream.next().await {
                    if event.is_terminal() {
                        assert!(matches!(event, AgentEvent::Error(_)));
                    }
                }
            })
            .await
            .unwrap();
        }
        tokio::time::timeout(Duration::from_secs(1), async {
            while model.active.load(Ordering::SeqCst) != 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("child request was dropped");
    }
}

#[test]
fn delegation_can_be_disabled_or_excluded_by_tool_selection() {
    let model = Arc::new(Model::new("explore"));
    let runs = Arc::new(AtomicUsize::new(0));
    let disabled = agent(
        Arc::clone(&model),
        AgentOptions::default().with_subagents(false),
        &runs,
    );
    assert!(
        !disabled
            .tool_names(Mode::Build)
            .contains(&"subagent".to_string())
    );
    let excluded = agent(
        model,
        AgentOptions::default().with_selection(ToolSelection {
            remove: vec!["subagent".into()],
            ..Default::default()
        }),
        &runs,
    );
    assert!(
        !excluded
            .tool_names(Mode::Build)
            .contains(&"subagent".to_string())
    );
    assert!(
        solaris_agent::agent_tool_registry()
            .unmatched(&ToolSelection {
                only: vec!["subagent".into()],
                ..Default::default()
            })
            .is_empty()
    );
}

#[tokio::test]
async fn child_sessions_persist_and_explicit_model_overrides_use_the_factory() {
    let directory = tempfile::tempdir().expect("directory");
    let store = solaris_agent::SessionStore::open(&directory.path().join("sessions.sqlite"))
        .expect("store");
    let journal = store
        .create(directory.path(), "test-model", None)
        .expect("parent");
    let parent_id = journal.id().to_string();
    let mut model = Model::new("explore");
    model.model_override = Some("child-model".into());
    let model = Arc::new(model);
    let child_model = Arc::clone(&model);
    let factory_called = Arc::new(AtomicUsize::new(0));
    let called = Arc::clone(&factory_called);
    let mut options = AgentOptions::in_directory(directory.path());
    options.child_backend = Some(Arc::new(move |name| {
        assert_eq!(name, "child-model");
        called.fetch_add(1, Ordering::SeqCst);
        Ok(BackendChoice {
            backend: child_model.clone(),
            provider_id: None,
            base_url: None,
            model: name.into(),
            source: CredentialSource::None,
        })
    }));
    let parent = agent(Arc::clone(&model), options, &Arc::new(AtomicUsize::new(0)));
    parent.attach_journal(journal).expect("attach");
    let events = run(&parent, Mode::Build).await;
    assert_eq!(factory_called.load(Ordering::SeqCst), 1);
    let sessions = store.list(directory.path()).expect("list");
    let child = sessions
        .iter()
        .find(|session| session.parent_id.as_deref() == Some(&parent_id))
        .expect("child session");
    assert_eq!(child.model, "child-model");
    assert!(events.iter().any(|event| matches!(event, AgentEvent::SubagentSession { session_id, .. } if session_id == &child.id)));
    let restored = store.resume(&child.id).expect("child is no longer running");
    assert!(
        restored
            .context()
            .expect("context")
            .iter()
            .any(|message| message.text().starts_with("You are an isolated"))
    );
    assert!(parent.active_tasks().is_empty());
}
