//! Real SDK round trips with a deterministic model, without network access.
use futures::StreamExt;
use serde_json::{Value, json};
use solaris_agent::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::time::{Duration, timeout};

struct Model {
    requests: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl ModelConnector for Model {
    async fn model_client(self: Arc<Self>) -> Result<ModelClient, BackendError> {
        let requests = self.requests.clone();
        Ok(test_support::client(Default::default(), move |request| {
            let number = requests.fetch_add(1, Ordering::SeqCst);
            async move {
                let events = if number == 0 {
                    assert!(request.tools.iter().any(|t| t.name == "question"));
                    vec![
                        AgentEvent::ToolCall(ToolCall::new("choose-storage", "question", input())),
                        complete(),
                    ]
                } else {
                    let result = request
                        .history
                        .iter()
                        .flat_map(|m| m.tool_results())
                        .find(|r| r.id == "choose-storage")
                        .expect("answer belongs to original tool call");
                    assert!(!result.is_error);
                    let response: Value = serde_json::from_str(&result.output).unwrap();
                    vec![
                        AgentEvent::TextDelta(format!("received {}", response["status"])),
                        complete(),
                    ]
                };
                Ok(Box::pin(futures::stream::iter(events)) as AgentEventStream)
            }
        }))
    }
}
fn complete() -> AgentEvent {
    AgentEvent::TurnComplete {
        usage: Usage::new(10, 1),
        cost_usd: 0.0,
    }
}
fn input() -> Value {
    json!({"questions":[{"id":"storage","header":"Storage","question":"Choose storage","options":[{"id":"sqlite","label":"SQLite","description":"Local"},{"id":"postgres","label":"PostgreSQL","description":"Shared"}],"allow_custom":true}]})
}
fn session(interactive: bool, requests: Arc<AtomicUsize>) -> SessionRuntime {
    let mut options = AgentOptions::default().with_subagents(false);
    if interactive {
        options.questions = Some(Questions::default());
    }
    SessionRuntime::new(Agent::new(
        ConnectionConfig {
            backend: Arc::new(Model { requests }),
            model: "fixture".into(),
            provider_id: None,
            fallback_context_window: 128_000,
            instructions: None,
        },
        Arc::new(ToolRegistry::with_tools(vec![])),
        options,
    ))
}
async fn start(session: &SessionRuntime) -> (RuntimeEventStream, QuestionRequest) {
    let mut stream = session
        .execute(RuntimeCommand::Prompt {
            text: "choose".into(),
            mode: Mode::Build,
        })
        .await
        .unwrap();
    let question = timeout(Duration::from_secs(5), async {
        loop {
            match stream.next().await.expect("event").event {
                AgentEvent::QuestionRequested(q) => break q,
                AgentEvent::Error(e) => panic!("{e}"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    (stream, question)
}
fn answer() -> QuestionResponse {
    QuestionResponse::Answered {
        answers: vec![QuestionAnswer {
            question_id: "storage".into(),
            selected: vec!["sqlite".into()],
            custom: None,
        }],
    }
}
async fn reply(
    session: &SessionRuntime,
    id: &str,
    response: QuestionResponse,
) -> Result<RuntimeSnapshot, String> {
    session
        .dispatch(RuntimeCommand::AnswerQuestion {
            request_id: id.into(),
            response,
        })?
        .accepted()
        .await
}
#[tokio::test]
async fn waits_for_user_then_continues_same_sdk_run_and_rejects_duplicate_reply() {
    let requests = Arc::new(AtomicUsize::new(0));
    let session = session(true, requests.clone());
    let (stream, q) = start(&session).await;
    assert_eq!(session.pending_questions(), vec![q.clone()]);
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    let invalid = QuestionResponse::Answered {
        answers: vec![QuestionAnswer {
            question_id: "storage".into(),
            selected: vec!["unknown".into()],
            custom: None,
        }],
    };
    assert!(reply(&session, &q.request_id, invalid).await.is_err());
    assert_eq!(session.pending_questions().len(), 1);
    reply(&session, &q.request_id, answer()).await.unwrap();
    assert!(reply(&session, &q.request_id, answer()).await.is_err());
    let events: Vec<_> = timeout(Duration::from_secs(5), stream.collect())
        .await
        .unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e.event, AgentEvent::QuestionResolved { .. }))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e.event, AgentEvent::TurnComplete { .. }))
    );
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    assert!(session.pending_questions().is_empty());
}
#[tokio::test]
async fn dismissal_is_returned_as_tool_feedback() {
    let session = session(true, Arc::new(AtomicUsize::new(0)));
    let (stream, q) = start(&session).await;
    reply(&session, &q.request_id, QuestionResponse::Dismissed)
        .await
        .unwrap();
    let events: Vec<_> = timeout(Duration::from_secs(5), stream.collect())
        .await
        .unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(&e.event,AgentEvent::TextDelta(s) if s.contains("dismissed")))
    );
}
#[tokio::test]
async fn cancellation_and_shutdown_remove_waiters_and_refuse_late_answers() {
    for shutdown in [false, true] {
        let session = session(true, Arc::new(AtomicUsize::new(0)));
        let (stream, q) = start(&session).await;
        if shutdown {
            session.shutdown();
        } else {
            session
                .dispatch(RuntimeCommand::Cancel)
                .unwrap()
                .accepted()
                .await
                .unwrap();
        }
        let _: Vec<_> = timeout(Duration::from_secs(5), stream.collect())
            .await
            .unwrap();
        assert!(session.pending_questions().is_empty());
        assert!(reply(&session, &q.request_id, answer()).await.is_err());
    }
}
#[tokio::test]
async fn answers_cannot_cross_sessions() {
    let first = session(true, Arc::new(AtomicUsize::new(0)));
    let second = session(true, Arc::new(AtomicUsize::new(0)));
    let (stream, q) = start(&first).await;
    assert!(reply(&second, &q.request_id, answer()).await.is_err());
    assert_eq!(first.pending_questions().len(), 1);
    reply(&first, &q.request_id, answer()).await.unwrap();
    let _: Vec<_> = timeout(Duration::from_secs(5), stream.collect())
        .await
        .unwrap();
}
#[test]
fn noninteractive_sessions_do_not_advertise_question_even_with_explicit_selection() {
    let options = AgentOptions::default().with_selection(ToolSelection {
        only: vec!["question".into()],
        ..Default::default()
    });
    let agent = Agent::new(
        ConnectionConfig {
            backend: Arc::new(Model {
                requests: Default::default(),
            }),
            model: "fixture".into(),
            provider_id: None,
            fallback_context_window: 128_000,
            instructions: None,
        },
        Arc::new(agent_tool_registry()),
        options,
    );
    assert!(!agent.tool_names(Mode::Build).contains(&"question".into()));
}

#[tokio::test]
async fn dropping_command_stream_cancels_pending_question() {
    let session = session(true, Arc::new(AtomicUsize::new(0)));
    let (stream, q) = start(&session).await;
    drop(stream);
    timeout(Duration::from_secs(5), async {
        while session.is_question_pending(&q.request_id) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(reply(&session, &q.request_id, answer()).await.is_err());
}

#[tokio::test]
async fn question_events_and_display_metadata_survive_replay_without_reopening_tools() {
    let directory = tempfile::tempdir().unwrap();
    let store = SessionStore::open(&directory.path().join("sessions.sqlite")).unwrap();
    let journal = store.create(directory.path(), "fixture", None).unwrap();
    let session = session(true, Arc::new(AtomicUsize::new(0)));
    session.attach_journal(journal.clone()).unwrap();
    let (stream, q) = start(&session).await;
    reply(&session, &q.request_id, answer()).await.unwrap();
    let _: Vec<_> = timeout(Duration::from_secs(5), stream.collect())
        .await
        .unwrap();
    let (_, sequence) = journal.history().unwrap();
    let mut replay = session.subscribe_from(0);
    let mut requested = false;
    let mut resolved = false;
    let mut metadata = false;
    while replay.sequence() < sequence {
        let event = replay.next().await.unwrap();
        match event.event {
            AgentEvent::QuestionRequested(_) => requested = true,
            AgentEvent::QuestionResolved { .. } => resolved = true,
            AgentEvent::ToolResult { result, .. } if result.id == "choose-storage" => {
                metadata = result.metadata.details["question"]["response"]["status"] == "answered"
            }
            _ => {}
        }
    }
    assert!(requested && resolved && metadata);
    assert!(session.pending_questions().is_empty());
}

#[path = "support/mod.rs"]
mod test_support;

#[path = "../src/context/estimate.rs"]
mod test_estimate;

use test_support::messages::MessageFixture;
