//! Resolve one immutable official model configuration before each request.
use crate::ResolvedModel;
use solaris_types::ModelCapabilities;
use std::sync::Arc;
/// A fresh immutable parameter set is taken at the start of each model request.
pub(crate) struct SelectedBackend {
    pub configuration: Arc<crate::ProviderConfig>,
    pub provider: String,
    pub model: String,
    pub prototype: solaris_backend::Backend,
    pub account_authorized: bool,
}
impl SelectedBackend {
    fn resolved(&self) -> ResolvedModel {
        let resolved = self.configuration.resolve(&self.provider, &self.model);
        if self.account_authorized {
            resolved.without_pricing()
        } else {
            resolved
        }
    }
    pub(crate) fn backend(&self) -> solaris_backend::Backend {
        let resolved = self.resolved();
        let wire = if self.account_authorized {
            solaris_backend::Wire::OpenAiResponses
        } else {
            self.configuration
                .wire(&self.provider, &resolved, self.prototype.wire())
        };
        self.prototype.clone().with_wire(wire).with_model_facts(
            resolved.request_id,
            resolved.output_tokens,
            resolved.facts,
            resolved.capabilities,
        )
    }
}

pub(crate) fn output_budget(capabilities: &ModelCapabilities, requested: u32) -> u32 {
    capabilities
        .max_output
        .unwrap_or(crate::DEFAULT_MAX_OUTPUT)
        .min(requested)
        .min(
            capabilities
                .context_window
                .unwrap_or(crate::DEFAULT_CONTEXT_WINDOW)
                .saturating_sub(1)
                .max(1),
        )
}

#[async_trait::async_trait]
impl solaris_backend::AgentBackend for SelectedBackend {
    async fn model_client(
        self: Arc<Self>,
    ) -> Result<solaris_backend::ModelClient, solaris_backend::BackendError> {
        Arc::new(self.backend()).model_client().await
    }
    fn label(&self) -> &str {
        self.prototype.label()
    }
    fn capabilities(&self) -> ModelCapabilities {
        self.resolved().capabilities
    }
    async fn run_turn(
        &self,
        request: solaris_backend::TurnRequest,
    ) -> Result<solaris_backend::AgentEventStream, solaris_backend::BackendError> {
        self.backend().run_turn(request).await
    }
    async fn models(&self) -> Result<Vec<String>, solaris_backend::BackendError> {
        self.prototype.models().await
    }
}
