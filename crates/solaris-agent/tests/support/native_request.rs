use crate::test_support::TurnRequest;
use futures::StreamExt;
use rig_core::serve::Serve;
use std::sync::Arc;

#[path = "display.rs"]
mod display;
#[path = "../../src/context/estimate.rs"]
mod estimate;

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
    let mut native = rig_core::completion::CompletionRequest::from(request.history);
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
