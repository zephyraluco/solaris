//! One immutable Rig connection with application accounting observers.
use crate::{Backend, BackendError, Usage};
use rig_core::effect::{EffectKind, family};
use rig_core::serve::{Dispatch, Reply, Serve, adapters::ModelAdapter};
type UsageObserver =
    std::sync::Arc<dyn Fn(Usage, Option<crate::Cost>) -> Result<(), BackendError> + Send + Sync>;
pub(crate) type StatusObservers =
    std::sync::Arc<std::sync::Mutex<Vec<std::sync::Arc<dyn Fn(String) + Send + Sync>>>>;
#[derive(Clone)]
pub struct ModelClient {
    adapter: rig_core::serve::ErasedHandler,
    capabilities: crate::ModelCapabilities,
    observers: Vec<UsageObserver>,
    status: StatusObservers,
}
impl ModelClient {
    pub(crate) fn native(backend: Backend) -> Result<Self, BackendError> {
        backend.check_credentials()?;
        let adapter = rig_core::serve::ErasedHandler::new(backend.model_adapter()?);
        let status = StatusObservers::default();
        Ok(Self {
            capabilities: backend.capabilities.clone(),
            adapter: rig_core::serve::ErasedHandler::new(NativeModel {
                backend,
                adapter,
                status: status.clone(),
            }),
            observers: Vec::new(),
            status,
        })
    }
    #[cfg(feature = "test-utils")]
    pub fn from_handler(
        handler: impl Serve<Family = family::Completion> + 'static,
        capabilities: crate::ModelCapabilities,
    ) -> Self {
        Self {
            adapter: rig_core::serve::ErasedHandler::new(handler),
            capabilities,
            observers: Vec::new(),
            status: StatusObservers::default(),
        }
    }
    pub fn capabilities(&self) -> crate::ModelCapabilities {
        self.capabilities.clone()
    }
    pub fn with_status_observer(self, observer: impl Fn(String) + Send + Sync + 'static) -> Self {
        self.status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(std::sync::Arc::new(observer));
        self
    }
    pub fn with_usage_observer(
        mut self,
        observer: impl Fn(Usage, Option<crate::Cost>) -> Result<(), BackendError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        self.observers.insert(0, std::sync::Arc::new(observer));
        self
    }
    /// Record usage and accumulate only charges supplied by the SDK.
    pub fn settle(
        &self,
        usage: Usage,
        reported_cost: Option<crate::Cost>,
    ) -> Result<f64, BackendError> {
        let cost = reported_cost.map_or(0.0, |cost| cost.total);
        for observer in &self.observers {
            observer(usage, reported_cost)?;
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
        self.adapter.handle(kind, dispatch).await
    }
}
struct NativeModel {
    backend: Backend,
    adapter: rig_core::serve::ErasedHandler,
    status: StatusObservers,
}
impl Serve for NativeModel {
    type Family = family::Completion;
    fn descriptor(&self) -> rig_core::effect::HandlerDescriptor {
        self.adapter.descriptor()
    }
    async fn serve(&self, kind: EffectKind, dispatch: Dispatch) -> Reply {
        match self.backend.prepare_effect(kind) {
            Ok(kind) => crate::request::execute(
                self.adapter.clone(),
                self.backend.clone(),
                kind,
                dispatch,
                self.status.clone(),
            ),
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
        if let Some(key) = self.prompt_cache_key {
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
    fn backend() -> Backend {
        backend_with_model(crate::ModelParameters::new(
            rig_core::catalog::ModelSpec::new(
                rig_core::providers::registry::ProviderId::declare("fixture").unwrap(),
                "test",
            )
            .with_tools(true)
            .with_max_output_tokens(100),
            Some(100),
            true,
        ))
    }
    fn backend_with_model(model: crate::ModelParameters) -> Backend {
        Backend::new(crate::TurnPlan {
            endpoint: crate::Endpoint {
                wire: crate::Wire::OpenAiResponses,
                session_header: None,
                base_url: "http://127.0.0.1:1/v1".into(),
                secret: None,
                name: "test".into(),
                label: "test".into(),
                env_keys: &[],
            },
            model,

            prompt_cache_key: None,
        })
        .unwrap()
    }
    #[test]
    fn settlement_uses_only_sdk_cost_and_preserves_missing_cost_for_observers() {
        use rig_core::catalog::{ModelSpec, Pricing};
        let mut spec = ModelSpec::new(
            rig_core::providers::registry::ProviderId::declare("test").unwrap(),
            "test",
        );
        spec.pricing = Some(Pricing::new(999.0, 999.0));
        let backend = backend_with_model(crate::ModelParameters::new(spec, Some(100), true));
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = seen.clone();
        let client = ModelClient::native(backend)
            .unwrap()
            .with_usage_observer(move |_, cost| {
                recorded.lock().unwrap().push(cost.map(|cost| cost.total));
                Ok(())
            });
        let usage = Usage::new(1_000_000, 100);
        assert_eq!(
            client
                .settle(usage, Some(crate::Cost::from_parts(0.25, 0.0, 0.0, 0.0)))
                .unwrap(),
            0.25
        );
        assert_eq!(
            client.settle(usage, None).unwrap(),
            0.0,
            "unknown cost adds no invented charge"
        );
        assert_eq!(*seen.lock().unwrap(), vec![Some(0.25), None]);
    }

    #[tokio::test]
    async fn invalid_extra_parameters_fail_before_network_instead_of_panicking() {
        let backend = backend();
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
