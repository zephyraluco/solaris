use async_trait::async_trait;
use futures::StreamExt;
use solaris_agent::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::sync::Semaphore;
struct Model {
    parent: AtomicUsize,
    background: bool,
    observed_followup: AtomicBool,
    started: Semaphore,
    release: Semaphore,
}
#[async_trait]
impl ModelConnector for Model {
    async fn model_client(self: Arc<Self>) -> Result<ModelClient, BackendError> {
        Ok(test_support::client(Default::default(), move |request| {
            let this = self.clone();
            async move {
                let child = request.history.iter().any(|m| {
                    matches!(m, rig_core::message::Message::User { .. })
                        && m.text().starts_with("You are an isolated")
                });
                let events = if child {
                    if request
                        .history
                        .iter()
                        .any(|m| m.text().contains("follow-up instructions"))
                    {
                        this.observed_followup.store(true, Ordering::SeqCst);
                    }
                    this.started.add_permits(1);
                    this.release.acquire().await.unwrap().forget();
                    vec![AgentEvent::TextDelta("child result".into())]
                } else if this.parent.fetch_add(1, Ordering::SeqCst) == 0 {
                    vec![AgentEvent::ToolCall(ToolCall::new(
                        "launch",
                        "subagent",
                        serde_json::json!({"agent":"explore","prompt":"inspect","background":this.background}),
                    ))]
                } else {
                    let received = request
                        .history
                        .iter()
                        .any(|m| m.text().contains("Background subagent notification"));
                    vec![AgentEvent::TextDelta(
                        if received {
                            "parent received"
                        } else {
                            "parent finished"
                        }
                        .into(),
                    )]
                };
                Ok(Box::pin(futures::stream::iter(events.into_iter().chain([
                    AgentEvent::TurnComplete {
                        usage: Usage::new(2, 1),
                        cost_usd: 0.0,
                    },
                ]))) as AgentEventStream)
            }
        }))
    }
}
fn model() -> Arc<Model> {
    Arc::new(Model {
        parent: AtomicUsize::new(0),
        background: true,
        observed_followup: AtomicBool::new(false),
        started: Semaphore::new(0),
        release: Semaphore::new(0),
    })
}

struct Nested;
#[async_trait]
impl ModelConnector for Nested {
    async fn model_client(self: Arc<Self>) -> Result<ModelClient, BackendError> {
        Ok(test_support::client(
            Default::default(),
            |request| async move {
                let delegated = request
                    .history
                    .iter()
                    .any(|message| message.tool_results().next().is_some());
                let leaf = request
                    .history
                    .iter()
                    .any(|message| message.text().contains("level-two"));
                let child = request
                    .history
                    .iter()
                    .any(|message| message.text().contains("level-one"));
                let mut events = if leaf || delegated {
                    vec![AgentEvent::TextDelta("finished".into())]
                } else {
                    vec![AgentEvent::ToolCall(ToolCall::new(
                        "nested",
                        "subagent",
                        serde_json::json!({"agent":"explore","prompt":if child {"level-two"} else {"level-one"}}),
                    ))]
                };
                events.push(AgentEvent::TurnComplete {
                    usage: Usage::new(2, 1),
                    cost_usd: 0.0,
                });
                Ok(Box::pin(futures::stream::iter(events)) as AgentEventStream)
            },
        ))
    }
}
#[tokio::test]
async fn nested_foreground_delegation_does_not_hold_the_only_execution_slot() {
    let mut options = AgentOptions {
        max_parallel: 1,
        ..Default::default()
    };
    options.execution.subagent_depth = 2;
    let session = SessionRuntime::new(Agent::new(
        ConnectionConfig {
            backend: Arc::new(Nested),
            model: "test".into(),
            provider_id: None,
            fallback_context_window: 128_000,
            instructions: None,
        },
        Arc::new(ToolRegistry::with_tools(vec![])),
        options,
    ));
    let events: Vec<_> = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        session
            .execute(RuntimeCommand::Prompt {
                text: "root".into(),
                mode: Mode::Build,
            })
            .await
            .unwrap()
            .collect()
            .await
    })
    .await
    .expect("nested delegation must release waiting capacity");
    assert!(
        events
            .iter()
            .any(|event| matches!(event.event, AgentEvent::TurnComplete { .. }))
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.event, AgentEvent::Error(_)))
    );
    assert_eq!(session.jobs().list(&session.snapshot().session_id).len(), 1);
}
fn agent(model: Arc<Model>) -> Agent {
    Agent::new(
        ConnectionConfig {
            backend: model,
            model: "test".into(),
            provider_id: None,
            fallback_context_window: 128_000,
            instructions: None,
        },
        Arc::new(ToolRegistry::with_tools(vec![])),
        AgentOptions::default(),
    )
}
#[tokio::test]
async fn background_job_outlives_parent_turn_and_delivers_to_idle_session() {
    let model = model();
    let runtime = Runtime::default();
    let session = runtime.open(agent(model.clone())).unwrap();
    let mut updates = session.subscribe();
    let events: Vec<_> = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        session
            .execute(RuntimeCommand::Prompt {
                text: "delegate".into(),
                mode: Mode::Build,
            })
            .await
            .unwrap()
            .collect()
            .await
    })
    .await
    .unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event.event, AgentEvent::TurnComplete { .. }))
    );
    model.started.acquire().await.unwrap().forget();
    let owner = session.snapshot().session_id;
    let jobs = runtime.jobs();
    let info = jobs.list(&owner).pop().unwrap();
    assert_eq!(info.status, JobStatus::Running);
    drop(session.execute(RuntimeCommand::Cancel).await.unwrap());
    assert_eq!(jobs.get(&info.id).unwrap().status, JobStatus::Running);
    model.release.add_permits(1);
    assert_eq!(
        jobs.wait(&info.id).await.unwrap().status,
        JobStatus::Completed
    );
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let event = updates.recv().await.unwrap();
            if matches!(&event.event,AgentEvent::TextDelta(text) if text=="parent received") {
                break;
            }
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn application_shutdown_marks_running_jobs_interrupted() {
    let model = model();
    let runtime = Runtime::default();
    let session = runtime.open(agent(model.clone())).unwrap();
    let _: Vec<_> = session
        .execute(RuntimeCommand::Prompt {
            text: "delegate".into(),
            mode: Mode::Build,
        })
        .await
        .unwrap()
        .collect()
        .await;
    model.started.acquire().await.unwrap().forget();
    let info = runtime
        .jobs()
        .list(&session.snapshot().session_id)
        .pop()
        .unwrap();
    runtime.shutdown();
    let result = runtime.jobs().wait(&info.id).await.unwrap();
    assert_eq!(result.status, JobStatus::Interrupted);
    assert_eq!(result.stop_reason, Some(StopReason::Shutdown));
}

#[tokio::test]
async fn explicit_job_cancel_stops_only_the_requested_child() {
    let model = model();
    let runtime = Runtime::default();
    let session = runtime.open(agent(model.clone())).unwrap();
    let _: Vec<_> = session
        .execute(RuntimeCommand::Prompt {
            text: "delegate".into(),
            mode: Mode::Build,
        })
        .await
        .unwrap()
        .collect()
        .await;
    model.started.acquire().await.unwrap().forget();
    let owner = session.snapshot().session_id;
    let info = runtime.jobs().list(&owner).pop().unwrap();
    assert!(!runtime.jobs().cancel("another-parent", &info.id));
    assert!(runtime.jobs().cancel(&owner, &info.id));
    let done = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        runtime.jobs().wait(&info.id),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(done.status, JobStatus::Cancelled);
    assert_eq!(done.stop_reason, Some(StopReason::UserCancelled));
}
#[tokio::test]
async fn cancelling_foreground_wait_cancels_the_child_job() {
    let mut fixture = model();
    Arc::get_mut(&mut fixture).unwrap().background = false;
    let runtime = Runtime::default();
    let session = runtime.open(agent(fixture.clone())).unwrap();
    let events = session
        .execute(RuntimeCommand::Prompt {
            text: "delegate".into(),
            mode: Mode::Build,
        })
        .await
        .unwrap();
    fixture.started.acquire().await.unwrap().forget();
    let id = runtime
        .jobs()
        .list(&session.snapshot().session_id)
        .pop()
        .unwrap()
        .id;
    drop(session.execute(RuntimeCommand::Cancel).await.unwrap());
    let _: Vec<_> = events.collect().await;
    let done = tokio::time::timeout(std::time::Duration::from_secs(2), runtime.jobs().wait(&id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(done.status, JobStatus::Cancelled);
}
#[tokio::test]
async fn restart_restores_interrupted_jobs_without_replaying_tools() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sessions.sqlite");
    let store = SessionStore::open(&path).unwrap();
    let journal = store.create(dir.path(), "test", None).unwrap();
    let owner = journal.id().to_owned();
    let fixture = model();
    let agent = agent(fixture.clone());
    agent.attach_journal(journal.clone()).unwrap();
    let runtime = Runtime::default();
    let session = runtime.open(agent).unwrap();
    let _: Vec<_> = session
        .execute(RuntimeCommand::Prompt {
            text: "delegate".into(),
            mode: Mode::Build,
        })
        .await
        .unwrap()
        .collect()
        .await;
    fixture.started.acquire().await.unwrap().forget();
    let id = runtime.jobs().list(&owner).pop().unwrap().id;
    runtime.shutdown();
    drop(session);
    drop(runtime);
    let restored = crate_agent(fixture.clone());
    restored.attach_journal(journal).unwrap();
    let runtime = Runtime::default();
    let session = runtime.open(restored).unwrap();
    assert_eq!(
        runtime.jobs().get(&id).unwrap().status,
        JobStatus::Interrupted
    );
    assert_eq!(fixture.started.available_permits(), 0);
    assert_eq!(session.snapshot().session_id, owner);
}
fn crate_agent(model: Arc<Model>) -> Agent {
    agent(model)
}

#[tokio::test]
async fn running_child_accepts_a_followup_without_starting_a_second_job() {
    let model = model();
    let runtime = Runtime::default();
    let session = runtime.open(agent(model.clone())).unwrap();
    let _: Vec<_> = session
        .execute(RuntimeCommand::Prompt {
            text: "delegate".into(),
            mode: Mode::Build,
        })
        .await
        .unwrap()
        .collect()
        .await;
    model.started.acquire().await.unwrap().forget();
    let owner = session.snapshot().session_id;
    let info = runtime.jobs().list(&owner).pop().unwrap();
    runtime
        .jobs()
        .send(&owner, &info.id, "follow-up instructions")
        .unwrap();
    model.release.add_permits(2);
    let done = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        runtime.jobs().wait(&info.id),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(done.status, JobStatus::Completed);
    assert!(model.observed_followup.load(Ordering::SeqCst));
    assert_eq!(runtime.jobs().list(&owner).len(), 1);
}

#[path = "support/mod.rs"]
mod test_support;

use test_support::messages::MessageFixture;
