//! Application-owned translation from provider resolution to agent inputs.

use solaris_agent::ConnectionConfig;
use solaris_provider::{BackendChoice, DEFAULT_CONTEXT_WINDOW, provider_spec};

/// Resolve provider catalogue metadata into the agent's execution contract.
/// Authentication provenance and endpoint configuration remain in the app.
pub fn agent_backend(choice: BackendChoice) -> ConnectionConfig {
    ConnectionConfig {
        fallback_context_window: choice
            .backend
            .capabilities()
            .context_window
            .unwrap_or(DEFAULT_CONTEXT_WINDOW),
        instructions: choice
            .provider_id
            .as_deref()
            .and_then(provider_spec)
            .and_then(|spec| spec.prompt)
            .map(str::to_owned),
        provider_id: choice.provider_id,
        backend: choice.backend,
        model: choice.model,
    }
}

use crate::application::connection::BackendFactory;
use crate::config::Config;
use solaris_agent::{Agent, AgentOptions, Approver, PromptContext, Skills};
use solaris_provider::AuthStore;
use solaris_tools::{ToolRegistry, ToolSelection};
use std::{path::Path, sync::Arc};

/// Resolve the backend for the current credentials, adopting the model that
/// will really be asked for.
///
/// Adopting matters as soon as a provider is connected: with no model named
/// yet, the resolution layer asks for the first model that provider offers, and
/// the app takes that on so the footer and `/stats` report what is really being
/// asked for. The context window follows the model, because the footer gauge is
/// only honest if it measures against the window the model really has.
/// The backend the current credentials call for, and why it was chosen.
pub(crate) fn resolve_backend(
    factory: &BackendFactory,
    auth: &AuthStore,
    config: &mut Config,
) -> BackendChoice {
    let choice = factory(auth, &config.model);
    if choice.provider_id.is_some() {
        config.model = choice.model.clone();
    }
    config.context_window = u64::from(
        choice
            .backend
            .capabilities()
            .context_window
            .unwrap_or(DEFAULT_CONTEXT_WINDOW),
    );
    choice
}

/// Wrap a resolved backend in the agent session.
///
/// Every backend goes through it — including the one that refuses turns when
/// nothing is connected — because the session is transparent until the model
/// asks for a tool: with none declared it is exactly one model call, forwarded
/// unchanged. The returned handle stops whichever turn the session is running.
#[expect(clippy::too_many_arguments, reason = "application assembly boundary")]
pub(crate) fn with_tools(
    choice: BackendChoice,
    registry: Arc<ToolRegistry>,
    selection: &ToolSelection,
    skills: &Skills,
    prompt_context: &PromptContext,
    cwd: &Path,
    approver: Arc<dyn Approver>,
    execution: &solaris_agent::ExecutionSettings,
    factory: &BackendFactory,
    auth: &AuthStore,
    runtime: &solaris_agent::Runtime,
) -> Result<Arc<solaris_agent::SessionRuntime>, solaris_agent::StoreError> {
    let mut options = AgentOptions::in_directory(cwd)
        .with_selection(selection.clone())
        .with_approver(approver);
    options.execution = execution.clone();
    let factory = Arc::clone(factory);
    let auth = auth.clone();
    let child_factory: solaris_agent::ChildBackendFactory =
        Arc::new(move |model| Ok(crate::agent_backend(factory(&auth, model))));
    let agent = Arc::new(
        runtime.open(
            Agent::new(crate::agent_backend(choice), registry, options)
                .with_resources(solaris_agent::ResourceSnapshot {
                    skills: skills.clone(),
                    prompt: prompt_context.clone(),
                    warnings: Vec::new(),
                })
                .with_child_backend(child_factory),
        )?,
    );
    Ok(agent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use solaris_agent::testing::TurnRequest;
    use solaris_agent::{AgentEventStream, BackendError, ModelConnector};
    use solaris_provider::CredentialSource;
    use std::sync::Arc;

    struct Backend;
    #[async_trait::async_trait]
    impl ModelConnector for Backend {
        async fn model_client(
            self: std::sync::Arc<Self>,
        ) -> Result<solaris_agent::ModelClient, solaris_agent::BackendError> {
            Ok(solaris_agent::testing::client(
                self.capabilities(),
                move |request| {
                    let fixture = std::sync::Arc::clone(&self);
                    async move { fixture.respond(request).await }
                },
            ))
        }
    }
    impl Backend {
        async fn respond(&self, _: TurnRequest) -> Result<AgentEventStream, BackendError> {
            Ok(Box::pin(futures::stream::empty()))
        }
    }

    #[test]
    fn application_resolves_endpoint_instructions_and_context_fallback() {
        let backend: Arc<dyn ModelConnector> = Arc::new(Backend);
        let config = agent_backend(BackendChoice {
            models: Arc::new(solaris_provider::UnavailableModels),
            configuration: Default::default(),
            backend: Arc::clone(&backend),
            provider_id: Some("local".into()),
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
            models: Arc::new(solaris_provider::UnavailableModels),
            configuration: Default::default(),
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
