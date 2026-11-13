//! Test fixtures for Rig handlers and real native HTTP connections. Not built in production.
mod display;
mod fixture;
use crate::{Message, ToolSpec};
pub use fixture::client;
use futures::StreamExt;
use rig_core::serve::Serve;
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
    backend: Arc<dyn crate::AgentBackend>,
    request: TurnRequest,
) -> Result<crate::AgentEventStream, crate::BackendError> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let client = backend
        .model_client()
        .await?
        .with_status_observer(move |text| {
            let _ = tx.send(crate::AgentEvent::Status(text));
        });
    let characters = request.prompt.chars().count()
        + request
            .history
            .iter()
            .map(|m| m.characters())
            .sum::<usize>();
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
                .map_err(|e| crate::BackendError::new(e.to_string()))?,
            tool.description,
            tool.parameters,
        ));
    }
    let request = native;
    let reply = client
        .serve(
            rig_core::effect::EffectKind::Completion {
                request: Arc::new(request),
                stream: true,
            },
            rig_core::serve::Dispatch::new(rig_core::effect::EffectId::from_raw(0), true),
        )
        .await;
    let mut events = display::adapt(reply.into_stream(), rx, characters);
    let mut opening = Vec::new();
    while let Some(event) = events.next().await {
        match event {
            crate::AgentEvent::Error(error) => return Err(crate::BackendError::new(error)),
            event @ crate::AgentEvent::Status(_) => opening.push(event),
            event => {
                opening.push(event);
                return Ok(Box::pin(futures::stream::iter(opening).chain(events)));
            }
        }
    }
    Err(crate::BackendError::new(
        "model stream ended without a complete response",
    ))
}

#[async_trait::async_trait]
pub trait NativeRequest: crate::AgentBackend + Clone + Sized + 'static {
    async fn complete(
        &self,
        input: TurnRequest,
    ) -> Result<crate::AgentEventStream, crate::BackendError> {
        request(Arc::new(self.clone()), input).await
    }
}
impl NativeRequest for crate::Backend {}
