//! Background delegation through the public API, synchronized without sleeps.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{Value, json};
use solaris_agent::{
    Agent, AgentBackend, AgentEvent, AgentEventStream, AgentOptions, BackendError, Mode, Tool,
    ToolCall, ToolContext, ToolOutput, ToolRegistry, TurnRequest, Usage,
};
use solaris_provider::{BackendChoice, CredentialSource};
use tokio::sync::{Notify, Semaphore};

#[derive(Default)]
struct Active {
    count: AtomicUsize,
    peak: AtomicUsize,
}

struct ActiveGuard<'a>(&'a Active);
impl Drop for ActiveGuard<'_> {
    fn drop(&mut self) {
        self.0.count.fetch_sub(1, Ordering::SeqCst);
    }
}

struct Model {
    inputs: Vec<Value>,
    seen: Mutex<Vec<TurnRequest>>,
    parent_rounds: AtomicUsize,
    started: Semaphore,
    release: Semaphore,
    progressed: Notify,
    idle: Notify,
    active: Active,
    fail_child: bool,
    panic_child: bool,
    fail_parent: bool,
    child_tool_before_wait: bool,
    hold_parent_request: bool,
    parent_release: Semaphore,
}

impl Model {
    fn new(count: usize) -> Self {
        Self {
            inputs: (0..count)
                .map(|index| {
                    json!({
                        "agent": "explore", "prompt": format!("Task {index}"), "background": true,
                    })
                })
                .collect(),
            seen: Mutex::default(),
            parent_rounds: AtomicUsize::new(0),
            started: Semaphore::new(0),
            release: Semaphore::new(0),
            progressed: Notify::new(),
            idle: Notify::new(),
            active: Active::default(),
            fail_child: false,
            panic_child: false,
            fail_parent: false,
            child_tool_before_wait: false,
            hold_parent_request: false,
            parent_release: Semaphore::new(0),
        }
    }

    fn requests(&self) -> Vec<TurnRequest> {
        self.seen.lock().unwrap().clone()
    }

    async fn wait_started(&self, count: u32) {
        self.started.acquire_many(count).await.unwrap().forget();
    }
}

fn child(request: &TurnRequest) -> bool {
    request
        .history
        .iter()
        .any(|message| message.text().starts_with("You are an isolated"))
}

fn notifications(request: &TurnRequest) -> Vec<String> {
    request
        .history
        .iter()
        .map(|message| message.text())
        .filter(|text| text.starts_with("Background subagent notification "))
        .collect()
}

#[async_trait]
impl AgentBackend for Model {
    async fn run_turn(&self, request: TurnRequest) -> Result<AgentEventStream, BackendError> {
        let is_child = child(&request);
        let child_answered = request
            .history
            .iter()
            .any(|message| message.tool_results().next().is_some());
        let received = notifications(&request);
        self.seen.lock().unwrap().push(request);
        let mut events = if is_child && self.child_tool_before_wait && !child_answered {
            vec![AgentEvent::ToolCall(ToolCall::new(
                "child-read",
                "read",
                json!({}),
            ))]
        } else if is_child {
            let count = self.active.count.fetch_add(1, Ordering::SeqCst) + 1;
            self.active.peak.fetch_max(count, Ordering::SeqCst);
            let _guard = ActiveGuard(&self.active);
            self.started.add_permits(1);
            self.release.acquire().await.unwrap().forget();
            assert!(!self.panic_child, "scripted child panic");
            if self.fail_child {
                return Err(BackendError::new("child backend unavailable"));
            }
            vec![AgentEvent::TextDelta("child final report".into())]
        } else {
            let round = self.parent_rounds.fetch_add(1, Ordering::SeqCst);
            if round == 0 {
                self.inputs
                    .iter()
                    .enumerate()
                    .map(|(index, input)| {
                        AgentEvent::ToolCall(ToolCall::new(
                            format!("launch-{index}"),
                            "subagent",
                            input.clone(),
                        ))
                    })
                    .collect()
            } else if round == 1 {
                if self.fail_parent {
                    self.wait_started(1).await;
                    return Err(BackendError::new("parent backend unavailable"));
                }
                self.progressed.notify_one();
                if self.hold_parent_request {
                    self.parent_release.acquire().await.unwrap().forget();
                }
                vec![AgentEvent::ToolCall(ToolCall::new(
                    "independent",
                    "read",
                    json!({}),
                ))]
            } else if received.len()
                == self
                    .inputs
                    .iter()
                    .filter(|input| input["background"] == true)
                    .count()
            {
                vec![AgentEvent::TextDelta(
                    "Parent integrated background reports.".into(),
                )]
            } else {
                self.idle.notify_one();
                vec![AgentEvent::TextDelta(
                    "Parent has finished independent work.".into(),
                )]
            }
        };
        events.push(AgentEvent::TurnComplete {
            usage: Usage::new(2, 1),
            cost_usd: 0.25,
        });
        Ok(Box::pin(futures::stream::iter(events)))
    }

    fn label(&self) -> &str {
        "background-script"
    }
}

#[derive(Default)]
struct Read(AtomicUsize);

#[async_trait]
impl Tool for Read {
    fn name(&self) -> &str {
        "read"
    }
    fn description(&self) -> &str {
        "Record independent parent work"
    }
    fn parameters(&self) -> Value {
        json!({"type": "object"})
    }
    async fn run(&self, _input: Value, _ctx: &ToolContext) -> ToolOutput {
        self.0.fetch_add(1, Ordering::SeqCst);
        ToolOutput::ok("independent work done")
    }
}

fn agent(model: &Arc<Model>, options: AgentOptions, read: &Arc<Read>) -> Agent {
    Agent::new(
        BackendChoice {
            backend: model.clone(),
            provider_id: None,
            base_url: None,
            model: "test-model".into(),
            source: CredentialSource::None,
        },
        Arc::new(ToolRegistry::with_tools(vec![read.clone()])),
        options,
    )
}

async fn run_with_release(model: &Arc<Model>, agent: &Agent) -> Vec<AgentEvent> {
    tokio::time::timeout(Duration::from_secs(3), async {
        let (events, ()) = tokio::join!(
            agent
                .run_turn("Delegate and continue working.", Mode::Build)
                .collect(),
            async {
                model.wait_started(1).await;
                model.progressed.notified().await;
                model.release.add_permits(model.inputs.len());
            }
        );
        events
    })
    .await
    .expect("background turn must finish")
}

fn completions(events: &[AgentEvent]) -> Vec<(&str, &str, bool)> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::SubagentComplete {
                task_id,
                output,
                is_error,
                ..
            } => Some((task_id.as_str(), output.as_str(), *is_error)),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_launch_returns_an_id_and_parent_work_runs_before_child_finishes() {
    let model = Arc::new(Model::new(1));
    let read = Arc::new(Read::default());
    let agent = agent(&model, AgentOptions::default(), &read);
    let events = run_with_release(&model, &agent).await;
    let requests = model.requests();
    let parent = requests
        .iter()
        .filter(|request| !child(request))
        .nth(1)
        .unwrap();
    let results: Vec<_> = parent
        .history
        .iter()
        .flat_map(|message| message.tool_results())
        .collect();
    assert_eq!(
        serde_json::from_str::<Value>(&results[0].output).unwrap(),
        json!({"task_id": "subagent-1", "status": "started"})
    );
    assert_eq!(read.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        completions(&events),
        vec![("subagent-1", "child final report", false)]
    );
    let final_request = requests
        .iter()
        .rev()
        .find(|request| !child(request))
        .unwrap();
    assert_eq!(notifications(final_request).len(), 1);
    // Completion is data, not a second ToolResult for the launch call.
    assert_eq!(
        final_request
            .history
            .iter()
            .flat_map(|m| m.tool_results())
            .filter(|result| result.id == "launch-0")
            .count(),
        1
    );
    let rounds = u32::try_from(requests.len()).unwrap();
    let child_usage = events
        .iter()
        .fold(Usage::default(), |total, event| match event {
            AgentEvent::SubagentUsage { usage, .. } => total.merge(*usage),
            _ => total,
        });
    assert_eq!(child_usage, Usage::new(2, 1));
    assert!(
        matches!(events.last(), Some(AgentEvent::TurnComplete { usage, cost_usd })
        if *usage == Usage::new(rounds * 2, rounds) && *cost_usd == f64::from(rounds) * 0.25)
    );
    assert_eq!(events.iter().filter(|event| event.is_terminal()).count(), 1);
}

#[tokio::test]
async fn foreground_default_and_explicit_false_wait_before_parent_work_continues() {
    for background in [None, Some(false)] {
        let mut model = Model::new(1);
        model.inputs[0]
            .as_object_mut()
            .unwrap()
            .remove("background");
        if let Some(background) = background {
            model.inputs[0]["background"] = json!(background);
        }
        let model = Arc::new(model);
        let read = Arc::new(Read::default());
        let agent = agent(&model, AgentOptions::default(), &read);
        let events = tokio::time::timeout(Duration::from_secs(3), async {
            let (events, ()) = tokio::join!(
                agent.run_turn("Delegate", Mode::Build).collect::<Vec<_>>(),
                async {
                    model.wait_started(1).await;
                    assert_eq!(model.parent_rounds.load(Ordering::SeqCst), 1);
                    assert_eq!(read.0.load(Ordering::SeqCst), 0);
                    model.release.add_permits(1);
                }
            );
            events
        })
        .await
        .unwrap();
        assert!(completions(&events).is_empty());
        assert!(events.iter().any(|event| matches!(event,
            AgentEvent::ToolResult { result, .. } if result.output == "child final report")));
        assert_eq!(read.0.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn an_idle_parent_waits_for_a_notification_without_polling_the_model() {
    let model = Arc::new(Model::new(1));
    let read = Arc::new(Read::default());
    let agent = agent(&model, AgentOptions::default(), &read);
    let events = tokio::time::timeout(Duration::from_secs(3), async {
        let (events, ()) = tokio::join!(
            agent.run_turn("Delegate", Mode::Build).collect::<Vec<_>>(),
            async {
                model.wait_started(1).await;
                model.idle.notified().await;
                for _ in 0..10 {
                    tokio::task::yield_now().await;
                }
                assert_eq!(model.parent_rounds.load(Ordering::SeqCst), 3);
                assert_eq!(read.0.load(Ordering::SeqCst), 1);
                model.release.add_permits(1);
            }
        );
        events
    })
    .await
    .unwrap();
    assert_eq!(completions(&events).len(), 1);
}

#[tokio::test]
async fn completion_notifies_immediately_even_while_the_parent_request_is_pending() {
    let mut model = Model::new(1);
    model.hold_parent_request = true;
    let model = Arc::new(model);
    let agent = agent(&model, AgentOptions::default(), &Arc::new(Read::default()));
    let mut stream = agent.run_turn("Delegate", Mode::Build);
    tokio::time::timeout(Duration::from_secs(1), async {
        model.wait_started(1).await;
        model.progressed.notified().await;
        model.release.add_permits(1);
        loop {
            if matches!(
                stream.next().await,
                Some(AgentEvent::SubagentComplete { .. })
            ) {
                break;
            }
        }
    })
    .await
    .expect("notification must not wait for the parent request");
    assert_eq!(model.parent_rounds.load(Ordering::SeqCst), 2);
    model.parent_release.add_permits(1);
    let rest: Vec<_> = tokio::time::timeout(Duration::from_secs(1), stream.collect())
        .await
        .unwrap();
    assert!(
        completions(&rest).is_empty(),
        "notification must not be duplicated at delivery"
    );
    let requests = model.requests();
    assert_eq!(notifications(requests.last().unwrap()).len(), 1);
}

#[tokio::test]
async fn steering_reaches_a_parent_waiting_for_background_reports() {
    let model = Arc::new(Model::new(1));
    let agent = agent(&model, AgentOptions::default(), &Arc::new(Read::default()));
    let mut stream = agent.run_turn("Delegate", Mode::Build);
    tokio::time::timeout(Duration::from_secs(1), async {
        model.wait_started(1).await;
        model.idle.notified().await;
        agent
            .steering_handle()
            .push("Continue with another independent check.");
        loop {
            if let Some(AgentEvent::Steered { text }) = stream.next().await {
                assert_eq!(text, "Continue with another independent check.");
                break;
            }
        }
    })
    .await
    .expect("steering must not wait for background work");
    assert_eq!(model.active.count.load(Ordering::SeqCst), 1);
    model.release.add_permits(1);
    let rest: Vec<_> = tokio::time::timeout(Duration::from_secs(1), stream.collect())
        .await
        .unwrap();
    assert_eq!(completions(&rest).len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_children_share_the_parallel_cap_across_batches() {
    let model = Arc::new(Model::new(5));
    let agent = agent(
        &model,
        AgentOptions {
            max_parallel: 2,
            ..Default::default()
        },
        &Arc::new(Read::default()),
    );
    let events = tokio::time::timeout(Duration::from_secs(3), async {
        let (events, ()) = tokio::join!(
            agent
                .run_turn("Delegate five tasks", Mode::Build)
                .collect::<Vec<_>>(),
            async {
                model.wait_started(2).await;
                model.progressed.notified().await;
                assert_eq!(model.active.count.load(Ordering::SeqCst), 2);
                model.release.add_permits(5);
            }
        );
        events
    })
    .await
    .unwrap();
    assert_eq!(model.active.peak.load(Ordering::SeqCst), 2);
    let mut ids: Vec<_> = completions(&events).iter().map(|entry| entry.0).collect();
    ids.sort_unstable();
    assert_eq!(
        ids,
        [
            "subagent-1",
            "subagent-2",
            "subagent-3",
            "subagent-4",
            "subagent-5"
        ]
    );
}

#[tokio::test]
async fn foreground_and_background_children_share_the_same_execution_slots() {
    let mut model = Model::new(2);
    model.inputs[1]["background"] = json!(false);
    let model = Arc::new(model);
    let agent = agent(
        &model,
        AgentOptions {
            max_parallel: 1,
            ..Default::default()
        },
        &Arc::new(Read::default()),
    );
    let events = tokio::time::timeout(Duration::from_secs(3), async {
        let (events, ()) = tokio::join!(
            agent.run_turn("Delegate", Mode::Build).collect::<Vec<_>>(),
            async {
                model.wait_started(1).await;
                assert_eq!(model.active.count.load(Ordering::SeqCst), 1);
                model.release.add_permits(1);
                model.wait_started(1).await;
                model.release.add_permits(1);
            }
        );
        events
    })
    .await
    .unwrap();
    assert_eq!(model.active.peak.load(Ordering::SeqCst), 1);
    assert_eq!(completions(&events).len(), 1);
}

#[tokio::test]
async fn failed_and_panicking_background_children_notify_the_parent_without_ending_its_turn() {
    for panic in [false, true] {
        let mut model = Model::new(1);
        model.fail_child = !panic;
        model.panic_child = panic;
        let model = Arc::new(model);
        let agent = agent(&model, AgentOptions::default(), &Arc::new(Read::default()));
        let events = run_with_release(&model, &agent).await;
        let reports = completions(&events);
        assert_eq!(reports.len(), 1);
        assert!(reports[0].2);
        assert!(reports[0].1.contains(if panic {
            "without completing"
        } else {
            "child backend unavailable"
        }));
        assert!(matches!(
            events.last(),
            Some(AgentEvent::TurnComplete { .. })
        ));
        assert_eq!(events.iter().filter(|event| event.is_terminal()).count(), 1);
    }
}

#[tokio::test]
async fn invalid_background_arguments_and_plan_writes_are_rejected_before_launch() {
    for input in [
        json!({"agent":"general", "prompt":"write", "background":true}),
        json!({"agent":"explore", "prompt":"read", "background":"true"}),
        json!({"agent":"explore", "prompt":" ", "background":true}),
    ] {
        let mut model = Model::new(1);
        model.inputs = vec![input];
        let model = Arc::new(model);
        let agent = agent(&model, AgentOptions::default(), &Arc::new(Read::default()));
        let mut stream = agent.run_turn("Delegate", Mode::Plan);
        let result = tokio::time::timeout(Duration::from_secs(1), async {
            while let Some(event) = stream.next().await {
                if let AgentEvent::ToolResult { result, .. } = event {
                    return result;
                }
            }
            panic!("launch must return an error")
        })
        .await
        .unwrap();
        assert!(result.is_error);
        assert!(!model.requests().iter().any(child));
        drop(stream);
    }
}

async fn wait_stopped(model: &Model) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while model.active.count.load(Ordering::SeqCst) != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("no background child may be left running");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_or_dropping_the_parent_stops_running_and_queued_background_children() {
    for drop_stream in [false, true] {
        let model = Arc::new(Model::new(3));
        let agent = agent(
            &model,
            AgentOptions {
                max_parallel: 1,
                ..Default::default()
            },
            &Arc::new(Read::default()),
        );
        let stream = agent.run_turn("Delegate", Mode::Build);
        tokio::time::timeout(Duration::from_secs(1), model.wait_started(1))
            .await
            .unwrap();
        if drop_stream {
            drop(stream);
        } else {
            agent.cancel_handle().cancel();
            let events: Vec<_> = tokio::time::timeout(Duration::from_secs(1), stream.collect())
                .await
                .unwrap();
            assert!(
                matches!(events.last(), Some(AgentEvent::Error(error)) if error.contains("cancelled"))
            );
        }
        wait_stopped(&model).await;
        model.release.add_permits(3);
        let next: Vec<_> = tokio::time::timeout(
            Duration::from_secs(1),
            agent.run_turn("A new turn", Mode::Build).collect(),
        )
        .await
        .unwrap();
        assert!(matches!(next.last(), Some(AgentEvent::TurnComplete { .. })));
        assert!(
            completions(&next).is_empty(),
            "old tasks must not leak into a new turn"
        );
        assert_eq!(
            model
                .requests()
                .iter()
                .filter(|request| child(request))
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn completed_child_request_usage_survives_parent_cancellation() {
    let mut model = Model::new(1);
    model.child_tool_before_wait = true;
    let model = Arc::new(model);
    let agent = agent(&model, AgentOptions::default(), &Arc::new(Read::default()));
    let stream = agent.run_turn("Delegate", Mode::Build);
    tokio::time::timeout(Duration::from_secs(1), model.wait_started(1))
        .await
        .unwrap();
    agent.cancel_handle().cancel();
    let events: Vec<_> = tokio::time::timeout(Duration::from_secs(1), stream.collect())
        .await
        .unwrap();
    let billed: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::SubagentUsage { usage, cost_usd } => Some((*usage, *cost_usd)),
            _ => None,
        })
        .collect();
    assert_eq!(billed, vec![(Usage::new(2, 1), 0.25)]);
    assert!(matches!(events.last(), Some(AgentEvent::Error(_))));
    wait_stopped(&model).await;
}

#[tokio::test]
async fn background_tool_display_events_are_linked_before_the_child_starts() {
    let mut model = Model::new(1);
    model.child_tool_before_wait = true;
    let model = Arc::new(model);
    let agent = agent(&model, AgentOptions::default(), &Arc::new(Read::default()));
    let events = run_with_release(&model, &agent).await;
    let started = events.iter().position(|event| matches!(event,
        AgentEvent::SubagentStarted { call_id, task_id } if call_id == "launch-0" && task_id == "subagent-1")).unwrap();
    let tool = events.iter().position(|event| matches!(event,
        AgentEvent::SubagentToolCall { task_id, call } if task_id == "subagent-1" && call.id == "child-read")).unwrap();
    assert!(started < tool);
    assert!(events.iter().any(|event| matches!(event,
        AgentEvent::SubagentToolResult { task_id, result, .. } if task_id == "subagent-1" && result.id == "child-read")));
    assert_eq!(completions(&events).len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parent_failure_aborts_its_background_children() {
    let mut model = Model::new(1);
    model.fail_parent = true;
    let model = Arc::new(model);
    let agent = agent(&model, AgentOptions::default(), &Arc::new(Read::default()));
    let events: Vec<_> = tokio::time::timeout(
        Duration::from_secs(1),
        agent.run_turn("Delegate", Mode::Build).collect(),
    )
    .await
    .unwrap();
    assert!(
        matches!(events.last(), Some(AgentEvent::Error(error)) if error.contains("parent backend unavailable"))
    );
    wait_stopped(&model).await;
}

#[tokio::test(start_paused = true)]
async fn background_timeout_is_a_notification_and_the_parent_can_finish() {
    let model = Arc::new(Model::new(1));
    let agent = agent(&model, AgentOptions::default(), &Arc::new(Read::default()));
    let mut stream = agent.run_turn("Delegate", Mode::Build);
    model.wait_started(1).await;
    model.idle.notified().await;
    tokio::time::advance(Duration::from_secs(301)).await;
    let events: Vec<_> = stream.by_ref().collect().await;
    let reports = completions(&events);
    assert_eq!(reports.len(), 1);
    assert!(reports[0].2 && reports[0].1.contains("timed out"));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnComplete { .. })
    ));
    wait_stopped(&model).await;
}
