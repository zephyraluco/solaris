//! Native model connection used by the application executor without event reconstruction.
use rig_core::effect::{EffectKind, family};
use rig_core::serve::{Dispatch, Reply, Serve, adapters::ModelAdapter};

use crate::{Backend, BackendError, Usage};

/// An opaque model connection. Provider configuration and SDK details stay internal.
#[derive(Clone)]
pub struct ModelClient {
    pub(crate) backend: Backend,
    pub(crate) adapter: std::sync::Arc<ModelAdapter<rig_core::operation::Completion>>,
}
impl ModelClient {
    /// Price a completed request using this connection's immutable metadata.
    pub fn cost(&self, usage: Usage) -> f64 {
        self.backend.usage_cost(usage)
    }
}
impl Serve for ModelClient {
    type Family = family::Completion;
    fn descriptor(&self) -> rig_core::effect::HandlerDescriptor {
        self.adapter.descriptor()
    }

    async fn serve(&self, kind: EffectKind, dispatch: Dispatch) -> Reply {
        match self.backend.prepare_effect(kind) {
            Ok(kind) => self.adapter.serve(kind, dispatch).await,
            Err(error) => Reply::Outcome(Err(rig_core::error::ErrorReport::new(
                rig_core::error::ErrorKind::Request,
                error.to_string(),
            ))),
        }
    }
}
impl Backend {
    pub(crate) fn model_adapter(
        &self,
    ) -> Result<ModelAdapter<rig_core::operation::Completion>, BackendError> {
        Ok(ModelAdapter::new("default", self.completion_model(&[])?))
    }
    fn prepare_effect(&self, kind: EffectKind) -> Result<EffectKind, BackendError> {
        let EffectKind::Completion { request, stream } = kind else {
            return Err(BackendError::new("expected a model request"));
        };
        self.check_credentials()?;
        if self.capabilities.tool_calling == crate::Support::Unsupported
            && !request.tools.is_empty()
        {
            return Err(BackendError::new(
                "this model does not support tool calling; choose another model or disable tools",
            ));
        }
        if self.capabilities.image_input == crate::Support::Unsupported
            && request
                .chat_history
                .iter()
                .any(|message| match message.as_ref() {
                    rig_core::message::Message::User { content } => {
                        content.iter().any(|part| match part {
                            rig_core::message::UserContent::Image(_) => true,
                            rig_core::message::UserContent::ToolResult(result) => {
                                result.content.iter().any(|part| {
                                    matches!(part, rig_core::message::ToolResultContent::Image(_))
                                })
                            }
                            _ => false,
                        })
                    }
                    rig_core::message::Message::Assistant(message) => message
                        .content
                        .iter()
                        .any(|part| matches!(part, rig_core::message::AssistantContent::Image(_))),
                    _ => false,
                })
        {
            return Err(BackendError::new("this model does not support image input"));
        }
        let mut request = std::sync::Arc::unwrap_or_clone(request);
        request.max_tokens = Some(u64::from(self.max_tokens));
        let mut params = request
            .additional_params
            .take()
            .unwrap_or_else(|| serde_json::json!({}));
        if self.endpoint.wire == crate::Wire::OpenAiResponses {
            params["store"] = false.into();
        }
        if let Some(key) = self.prompt_cache_key
            && self.capabilities.prompt_caching != crate::Support::Unsupported
        {
            params["prompt_cache_key"] = key.into();
        }
        request.additional_params = Some(params);
        Ok(EffectKind::Completion {
            request: std::sync::Arc::new(request),
            stream,
        })
    }
}
