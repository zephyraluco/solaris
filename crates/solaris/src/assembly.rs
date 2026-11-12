//! Application-owned translation from provider resolution to agent inputs.

use solaris_agent::AgentBackendConfig;
use solaris_provider::{BackendChoice, DEFAULT_CONTEXT_WINDOW, provider_spec};

/// Resolve provider catalogue metadata into the agent's execution contract.
/// Authentication provenance and endpoint configuration remain in the app.
pub fn agent_backend(choice: BackendChoice) -> AgentBackendConfig {
    AgentBackendConfig {
        fallback_context_window: choice
            .backend
            .capabilities()
            .context_window
            .unwrap_or(DEFAULT_CONTEXT_WINDOW),
        instructions: choice
            .provider_id
            .and_then(provider_spec)
            .and_then(|spec| spec.prompt)
            .map(str::to_owned),
        provider_id: choice.provider_id.map(str::to_owned),
        backend: choice.backend,
        model: choice.model,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solaris_agent::{AgentBackend, AgentEventStream, BackendError, TurnRequest};
    use solaris_provider::CredentialSource;
    use std::sync::Arc;

    struct Backend;
    #[async_trait::async_trait]
    impl AgentBackend for Backend {
        async fn model_client(
            self: std::sync::Arc<Self>,
        ) -> Result<solaris_agent::ModelClient, solaris_agent::BackendError> {
            Ok(solaris_agent::ModelClient::from_events(self))
        }

        async fn run_turn(&self, _: TurnRequest) -> Result<AgentEventStream, BackendError> {
            Ok(Box::pin(futures::stream::empty()))
        }
    }

    #[test]
    fn application_resolves_endpoint_instructions_and_context_fallback() {
        let backend: Arc<dyn AgentBackend> = Arc::new(Backend);
        let config = agent_backend(BackendChoice {
            backend: Arc::clone(&backend),
            provider_id: Some("local"),
            base_url: Some("http://localhost:8080/v1".into()),
            model: "claude-sonnet-4-5".into(),
            source: CredentialSource::None,
        });
        assert!(Arc::ptr_eq(&config.backend, &backend));
        assert_eq!(config.provider_id.as_deref(), Some("local"));
        assert_eq!(config.model, "claude-sonnet-4-5");
        assert_eq!(config.fallback_context_window, DEFAULT_CONTEXT_WINDOW);
        assert_eq!(
            config.instructions.as_deref(),
            provider_spec("local").unwrap().prompt
        );
        assert!(config.instructions.is_some());
    }

    #[test]
    fn unresolved_provider_does_not_add_endpoint_instructions() {
        let config = agent_backend(BackendChoice {
            backend: Arc::new(Backend),
            provider_id: None,
            base_url: None,
            model: "custom-model".into(),
            source: CredentialSource::Missing,
        });
        assert!(config.provider_id.is_none());
        assert!(config.instructions.is_none());
        assert_eq!(config.model, "custom-model");
        assert_eq!(config.fallback_context_window, DEFAULT_CONTEXT_WINDOW);
    }
}
