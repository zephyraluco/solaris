use async_trait::async_trait;
use futures::StreamExt;
use solaris_agent::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Probe(Arc<AtomicUsize>);
#[async_trait]
impl Tool for Probe {
    fn name(&self) -> &str {
        "probe"
    }
    fn description(&self) -> &str {
        "read-only test probe"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object"})
    }
    fn is_read_only(&self) -> bool {
        true
    }
    async fn run(&self, _: serde_json::Value, _: &ToolContext) -> ToolOutput {
        self.0.fetch_add(1, Ordering::SeqCst);
        ToolOutput::ok("observed")
    }
}
struct Model(Arc<AtomicUsize>);
#[async_trait]
impl ModelConnector for Model {
    async fn model_client(self: Arc<Self>) -> Result<ModelClient, BackendError> {
        Ok(test_support::client(Default::default(), move |request| {
            let count = self.0.fetch_add(1, Ordering::SeqCst);
            async move {
                let mut events = Vec::new();
                if request.tools.is_empty() || count >= 24 {
                    events.push(AgentEvent::TextDelta("phase report".into()));
                } else {
                    events.push(AgentEvent::ToolCall(ToolCall::new(
                        format!("call-{count}"),
                        "probe",
                        serde_json::json!({}),
                    )));
                }
                events.push(AgentEvent::TurnComplete {
                    usage: Usage::new(2, 1),
                    cost_usd: 0.0,
                });
                Ok(Box::pin(futures::stream::iter(events)) as AgentEventStream)
            }
        }))
    }
}
async fn execute(steps: Option<std::num::NonZeroU32>) -> (usize, usize, Vec<RuntimeEvent>) {
    let requests = Arc::new(AtomicUsize::new(0));
    let tools = Arc::new(AtomicUsize::new(0));
    let mut options = AgentOptions::default()
        .with_subagents(false)
        .with_selection(ToolSelection {
            only: vec!["probe".into()],
            ..Default::default()
        });
    options.execution.steps = steps;
    let runtime = SessionRuntime::new(Agent::new(
        ConnectionConfig {
            backend: Arc::new(Model(requests.clone())),
            model: "test".into(),
            provider_id: None,
            fallback_context_window: 128_000,
            instructions: None,
        },
        Arc::new(ToolRegistry::with_tools(vec![Arc::new(Probe(
            tools.clone(),
        ))])),
        options,
    ));
    let events = runtime
        .execute(RuntimeCommand::Prompt {
            text: "inspect".into(),
            mode: Mode::Build,
        })
        .await
        .unwrap()
        .collect()
        .await;
    (
        requests.load(Ordering::SeqCst),
        tools.load(Ordering::SeqCst),
        events,
    )
}
#[tokio::test]
async fn optional_steps_end_with_a_text_only_report() {
    let (requests, tools, events) = execute(std::num::NonZeroU32::new(3)).await;
    assert_eq!(requests, 3);
    assert_eq!(tools, 2);
    assert!(
        events
            .iter()
            .any(|event| matches!(event.event, AgentEvent::TurnComplete { .. }))
    );
    assert!(
        events.iter().any(
            |event| matches!(&event.event,AgentEvent::TextDelta(text) if text=="phase report")
        )
    );
}
#[tokio::test]
async fn unspecified_steps_do_not_impose_a_twenty_request_ceiling() {
    let (requests, tools, events) = execute(None).await;
    assert_eq!(requests, 25);
    assert_eq!(tools, 24);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.event, AgentEvent::Error(_)))
    );
}

#[path = "support/mod.rs"]
mod test_support;

#[path = "../src/context/estimate.rs"]
mod test_estimate;
