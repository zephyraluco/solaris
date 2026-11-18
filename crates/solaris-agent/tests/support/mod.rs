#![allow(dead_code)]
use crate::test_estimate as estimate;

mod display;
mod fixture;
pub use fixture::client;
use futures::StreamExt;
use rig_core::serve::Serve;
use solaris_agent::{Message, ToolSpec};
use std::sync::Arc;
/// Input observed by scripted Rig test fixtures.
///
/// The mode a turn runs in is deliberately absent: which tools a turn may use is
/// the agent's decision, and by the time a request reaches a backend its tool
/// list is already settled.
#[derive(Debug, Clone)]
pub struct TurnRequest {
    /// Prior conversation, oldest first, excluding the new prompt.
    pub history: Vec<Message>,
    /// The new user prompt.
    ///
    /// Empty means the caller has already put the prompt in `history` — which
    /// is what a tool round trip does — and the request bodies then append no
    /// further user message.
    pub prompt: String,
    /// Tools declared to the model for this request. Empty means none, and the
    /// request bodies then carry no `tools` field at all.
    pub tools: Vec<ToolSpec>,
}

impl TurnRequest {
    /// A request that declares no tools.
    pub fn new(history: Vec<Message>, prompt: impl Into<String>) -> Self {
        Self {
            history,
            prompt: prompt.into(),
            tools: Vec::new(),
        }
    }
}

/// Execute a fixture request through the sole Rig connection interface.
pub async fn request(
    backend: Arc<dyn solaris_agent::ModelConnector>,
    request: TurnRequest,
) -> Result<solaris_agent::AgentEventStream, solaris_backend::BackendError> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let client = backend
        .model_client()
        .await?
        .with_status_observer(move |text| {
            let _ = tx.send(solaris_agent::AgentEvent::Status(text));
        });
    let mut messages = request
        .history
        .iter()
        .map(rig_core::message::Message::try_from)
        .collect::<Result<Vec<_>, _>>()?;
    if !request.prompt.is_empty() {
        messages.push(rig_core::message::Message::user(request.prompt));
    }
    let mut native = rig_core::completion::CompletionRequest::from(messages);
    for tool in request.tools {
        native = native.tool(rig_core::completion::ToolDefinition::new(
            rig_core::message::ToolName::new(tool.name)
                .map_err(|e| solaris_backend::BackendError::new(e.to_string()))?,
            tool.description,
            tool.parameters,
        ));
    }
    let request = native;
    let input_tokens = estimate::request_tokens(&request);
    let reply = client
        .serve(
            rig_core::effect::EffectKind::Completion {
                request: Arc::new(request),
                stream: true,
            },
            rig_core::serve::Dispatch::new(rig_core::effect::EffectId::from_raw(0), true),
        )
        .await;
    let mut events = display::adapt(reply.into_stream(), rx, input_tokens);
    let mut opening = Vec::new();
    while let Some(event) = events.next().await {
        match event {
            solaris_agent::AgentEvent::Error(error) => {
                return Err(solaris_backend::BackendError::new(error));
            }
            event @ solaris_agent::AgentEvent::Status(_) => opening.push(event),
            event => {
                opening.push(event);
                return Ok(Box::pin(futures::stream::iter(opening).chain(events)));
            }
        }
    }
    Err(solaris_backend::BackendError::new(
        "model stream ended without a complete response",
    ))
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
