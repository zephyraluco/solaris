#![allow(dead_code)]

mod fixture;
pub use fixture::client;
use futures::StreamExt;
use rig_core::message::Message;
use solaris_agent::ToolSpec;
pub mod messages;
use std::sync::Arc;
/// Input observed by scripted Rig test fixtures.
///
/// The mode a turn runs in is deliberately absent: which tools a turn may use is
/// the agent's decision, and by the time a request reaches a backend its tool
/// list is already settled.
#[derive(Debug, Clone)]
pub struct TurnRequest {
    /// Messages sent to the model, oldest first.
    pub history: Vec<Message>,
    /// Tools declared to the model for this request. Empty means none, and the
    /// request bodies then carry no `tools` field at all.
    pub tools: Vec<ToolSpec>,
}

impl TurnRequest {
    /// The last nonempty user message sent to the model.
    pub fn last_user_text(&self) -> String {
        use messages::MessageFixture;

        self.history
            .iter()
            .rev()
            .filter(|message| matches!(message, Message::User { .. }))
            .map(MessageFixture::text)
            .find(|text| !text.is_empty())
            .unwrap_or_default()
    }

    /// A request that declares no tools.
    pub fn new(history: Vec<Message>) -> Self {
        Self {
            history,
            tools: Vec::new(),
        }
    }
}

/// Explicit native facts for unpriced HTTP test models.
pub fn parameters(id: &str, output_tokens: u32) -> solaris_backend::ModelParameters {
    let mut spec = rig_core::catalog::ModelSpec::new(
        rig_core::providers::registry::ProviderId::declare("fixture").unwrap(),
        id,
    )
    .with_tools(true)
    .with_max_output_tokens(output_tokens);
    spec.input.image = true;
    solaris_backend::ModelParameters::new(spec, Some(output_tokens), true)
}

pub async fn run_turn(
    session: &solaris_agent::SessionRuntime,
    text: &str,
    mode: solaris_agent::Mode,
) -> solaris_agent::AgentEventStream {
    Box::pin(
        session
            .execute(solaris_agent::RuntimeCommand::Prompt {
                text: text.into(),
                mode,
            })
            .await
            .unwrap()
            .map(|event| event.event),
    )
}

pub async fn compact(session: &solaris_agent::SessionRuntime) -> Option<usize> {
    let mut stream = session
        .execute(solaris_agent::RuntimeCommand::Compact)
        .await
        .unwrap();
    let mut compacted = None;
    while let Some(event) = stream.next().await {
        if let solaris_agent::AgentEvent::Compacted { dropped } = event.event {
            compacted = Some(dropped);
        }
    }
    compacted
}

use solaris_agent::{AgentEvent, AgentEventStream, BackendError, ModelConnector};
use std::{collections::VecDeque, sync::Mutex};
pub struct Scripted {
    rounds: Mutex<VecDeque<Vec<AgentEvent>>>,
    seen: Mutex<Vec<TurnRequest>>,
}

impl Scripted {
    pub fn new(rounds: Vec<Vec<AgentEvent>>) -> Arc<Self> {
        Arc::new(Self {
            rounds: Mutex::new(rounds.into()),
            seen: Mutex::new(Vec::new()),
        })
    }

    pub fn requests(&self) -> Vec<TurnRequest> {
        self.seen.lock().expect("lock").clone()
    }
}

#[async_trait::async_trait]
impl ModelConnector for Scripted {
    async fn model_client(
        self: std::sync::Arc<Self>,
    ) -> Result<solaris_backend::ModelClient, solaris_backend::BackendError> {
        Ok(client(self.capabilities(), move |request| {
            let fixture = std::sync::Arc::clone(&self);
            async move { fixture.respond(request).await }
        }))
    }

    fn label(&self) -> &str {
        "scripted"
    }
}
impl Scripted {
    async fn respond(&self, request: TurnRequest) -> Result<AgentEventStream, BackendError> {
        self.seen.lock().expect("lock").push(request);
        let events = self
            .rounds
            .lock()
            .expect("lock")
            .pop_front()
            .unwrap_or_else(|| vec![AgentEvent::Error("the script ran out".to_string())]);
        Ok(Box::pin(futures::stream::iter(events)))
    }
}
