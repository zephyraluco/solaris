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
    backend: Arc<dyn crate::ModelConnector>,
    request: TurnRequest,
) -> Result<crate::AgentEventStream, solaris_backend::BackendError> {
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
                .map_err(|e| solaris_backend::BackendError::new(e.to_string()))?,
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
            crate::AgentEvent::Error(error) => {
                return Err(solaris_backend::BackendError::new(error));
            }
            event @ crate::AgentEvent::Status(_) => opening.push(event),
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

#[async_trait::async_trait]
pub trait NativeRequest: crate::ModelConnector + Clone + Sized + 'static {
    async fn complete(
        &self,
        input: TurnRequest,
    ) -> Result<crate::AgentEventStream, solaris_backend::BackendError> {
        request(Arc::new(self.clone()), input).await
    }
}
impl NativeRequest for solaris_backend::Backend {}

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

/// Direct executor access exists only in test fixtures, not the production API.
pub trait AgentFixture {
    fn run_fixture(&self, prompt: impl Into<String>, mode: crate::Mode) -> crate::AgentEventStream;
    fn compact_fixture(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<usize>> + Send + '_>>;
}
impl AgentFixture for crate::Agent {
    fn run_fixture(&self, prompt: impl Into<String>, mode: crate::Mode) -> crate::AgentEventStream {
        self.run_turn(prompt, mode)
    }
    fn compact_fixture(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<usize>> + Send + '_>> {
        Box::pin(self.compact())
    }
}
