//! Native model connection used by the application executor without event reconstruction.
use rig_core::effect::{EffectKind, family};
use rig_core::serve::{Dispatch, Reply, Serve, adapters::ModelAdapter};

use crate::{Backend, BackendError, Usage};

pub(crate) type ReportedUsage = std::sync::Arc<std::sync::Mutex<Option<(Usage, f64)>>>;

type UsageObserver = std::sync::Arc<dyn Fn(Usage, f64) -> Result<(), BackendError> + Send + Sync>;

/// An immutable request connection. Providers and SDK details stay internal.
#[derive(Clone)]
pub struct ModelClient {
    pub(crate) backend: Option<Backend>,
    pub(crate) adapter: rig_core::serve::ErasedHandler,
    capabilities: crate::ModelCapabilities,
    reported: Option<ReportedUsage>,
    observers: Vec<UsageObserver>,
}
impl ModelClient {
    pub(crate) fn native(backend: Backend) -> Result<Self, BackendError> {
        backend.check_credentials()?;
        let adapter = rig_core::serve::ErasedHandler::new(backend.model_adapter()?);
        Ok(Self {
            capabilities: backend.capabilities.clone(),
            backend: Some(backend),
            adapter,
            reported: None,
            observers: Vec::new(),
        })
    }
    /// Explicit adapter for application-injected event sources and test doubles.
    pub fn from_events<T: crate::AgentBackend + ?Sized + 'static>(
        backend: std::sync::Arc<T>,
    ) -> Self {
        let reported = std::sync::Arc::new(std::sync::Mutex::new(None));
        Self {
            capabilities: backend.capabilities(),
            backend: None,
            adapter: rig_core::serve::ErasedHandler::new(crate::injected::EventModel {
                backend,
                reported: std::sync::Arc::clone(&reported),
            }),
            reported: Some(reported),
            observers: Vec::new(),
        }
    }
    pub fn capabilities(&self) -> crate::ModelCapabilities {
        self.capabilities.clone()
    }
    pub fn cost(&self, usage: Usage) -> f64 {
        match &self.backend {
            Some(backend) => backend.usage_cost(usage),
            None => self
                .reported
                .as_ref()
                .and_then(|report| *report.lock().unwrap_or_else(|p| p.into_inner()))
                .map_or(0.0, |(_, cost)| cost),
        }
    }
    /// Preserve an injected source's estimate marker, which SDK counters do not carry.
    pub fn usage(&self, measured: Usage) -> Usage {
        self.reported
            .as_ref()
            .and_then(|report| *report.lock().unwrap_or_else(|p| p.into_inner()))
            .map_or(measured, |(usage, _)| usage)
    }
    /// Add a product accounting policy without adding it to the provider trait.
    pub fn with_usage_observer(
        mut self,
        observer: impl Fn(Usage, f64) -> Result<(), BackendError> + Send + Sync + 'static,
    ) -> Self {
        self.observers.insert(0, std::sync::Arc::new(observer));
        self
    }
    /// Settle completed usage through the policies attached to this request.
    pub fn settle(&self, usage: Usage) -> Result<f64, BackendError> {
        let cost = self.cost(usage);
        for observer in &self.observers {
            observer(usage, cost)?;
        }
        Ok(cost)
    }
}
impl Serve for ModelClient {
    type Family = family::Completion;
    fn descriptor(&self) -> rig_core::effect::HandlerDescriptor {
        self.adapter.descriptor()
    }
    async fn serve(&self, kind: EffectKind, dispatch: Dispatch) -> Reply {
        let Some(backend) = &self.backend else {
            return self.adapter.handle(kind, dispatch).await;
        };
        match backend.prepare_effect(kind) {
            Ok(kind) => {
                crate::request::execute(self.clone(), backend.clone(), kind, dispatch, None)
            }
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
    pub(crate) fn prepare_effect(&self, kind: EffectKind) -> Result<EffectKind, BackendError> {
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
        request.max_tokens = Some(
            request
                .max_tokens
                .unwrap_or(u64::from(self.max_tokens))
                .min(u64::from(self.max_tokens)),
        );
        let mut params = match request.additional_params.take() {
            None | Some(serde_json::Value::Null) => serde_json::json!({}),
            Some(value @ serde_json::Value::Object(_)) => value,
            Some(_) => {
                return Err(BackendError::new(
                    "additional request parameters must be an object",
                ));
            }
        };
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

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn invalid_extra_parameters_fail_before_network_instead_of_panicking() {
        let backend = Backend::new(crate::TurnPlan {
            endpoint: crate::Endpoint {
                wire: crate::Wire::OpenAiResponses,
                session_header: None,
                base_url: "http://127.0.0.1:1/v1".into(),
                secret: None,
                name: "test".into(),
                label: "test".into(),
                env_keys: &[],
            },
            model: "test".into(),
            max_tokens: 100,

            prompt_cache_key: None,
        })
        .unwrap();
        let request = rig_core::completion::CompletionRequest::new("hello")
            .additional_params(serde_json::json!(["invalid"]));
        let error = backend
            .prepare_effect(EffectKind::Completion {
                request: std::sync::Arc::new(request),
                stream: true,
            })
            .unwrap_err();
        assert!(error.to_string().contains("must be an object"));
    }
}
