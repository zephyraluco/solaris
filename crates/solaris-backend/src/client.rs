//! Rig-backed completion calls with Solaris endpoint, retry and billing policy.
use crate::{
    AgentBackend, AgentEventStream, BackendError, Price, Support, TurnRequest, Usage, Wire,
};
use crate::{convert, http, transport::CompletionRoute};
use futures::StreamExt;
use rig_core::driver::{DynModel, Model};
use rig_core::operation::Completion;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// Where one turn goes, how it authenticates, and which protocol to speak.
///
/// Fixed at the moment the backend is built: nothing here is looked up later.
#[derive(Debug, Clone)]
pub struct Endpoint {
    /// Protocol to speak.
    pub wire: Wire,
    /// Header the platform wants a conversation id in, when it asks for one.
    pub session_header: Option<&'static str>,
    /// Base URL, without a trailing slash.
    pub base_url: String,
    /// The key or token to send, or `None` for a runtime that wants none.
    pub secret: Option<String>,
    /// Display name, used in error messages.
    pub name: &'static str,
    /// Short label shown in the status bar — the platform id.
    pub label: &'static str,
    /// Environment variables that could carry a key, for the error message.
    pub env_keys: &'static [&'static str],
}

impl Endpoint {
    /// The URL one turn is posted to.
    ///
    /// The API version is part of the base URL, so a gateway that serves several
    /// protocols under one root — opencode go answers `/responses`, `/messages`
    /// and `/chat/completions` beneath `/zen/go/v1` — needs no more than one entry
    /// in this match per protocol it speaks.
    pub fn url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        match self.wire {
            Wire::AnthropicMessages => format!("{base}/messages"),
            Wire::OpenAiChat => format!("{base}/chat/completions"),
            Wire::OpenAiResponses => format!("{base}/responses"),
        }
    }

    /// The URL the model list is fetched from.
    ///
    /// Both protocols expose the same `{"data": [{"id": …}]}` shape. The
    /// Messages API pages its list at twenty by default, so the largest page it
    /// allows is asked for; a list longer than that still shows its first page.
    pub fn models_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        match self.wire {
            Wire::AnthropicMessages => format!("{base}/models?limit=1000"),
            Wire::OpenAiChat | Wire::OpenAiResponses => format!("{base}/models"),
        }
    }
}

/// An id for this run, stable for as long as the process lives.
///
/// One process is one conversation here, which is exactly the granularity a
/// platform routing by session asks for.
pub(super) fn session_id() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        format!("solaris-{nanos:x}-{:x}", std::process::id())
    })
}

/// One turn, already resolved by the caller.
///
/// Everything platform-specific has been decided before this point: which
/// endpoint, which model, how large a reply to ask for, what that model costs,
/// and whether a prompt-cache key should ride along.
#[derive(Debug, Clone)]
pub struct TurnPlan {
    /// Where the turn goes and how it authenticates.
    pub endpoint: Endpoint,
    /// Model id to send, exactly as the caller wants it.
    pub model: String,
    /// Largest reply to ask for.
    pub max_tokens: u32,
    /// List price for the model, when the caller knows one. The backend only
    /// multiplies; it has no idea what any model costs by itself.
    pub price: Option<Price>,
    /// Prompt-cache routing key, for the platforms that want one.
    pub prompt_cache_key: Option<&'static str>,
}

/// Streams one turn from an HTTP endpoint, over whichever wire it speaks.
#[derive(Clone)]
pub struct Backend {
    pub(super) client: reqwest::Client,
    pub(super) endpoint: Endpoint,
    pub(super) model: String,
    pub(super) max_tokens: u32,
    pub(super) price: Option<Price>,
    pub(super) price_tiers: Vec<crate::PriceTier>,
    pub(super) prompt_cache_key: Option<&'static str>,
    pub(super) chatgpt: Option<(String, u64)>,
    pub(super) capabilities: crate::ModelCapabilities,
}

impl Backend {
    pub(super) fn usage_cost(&self, usage: Usage) -> f64 {
        self.price.map_or(0.0, |price| {
            price.cost_with_tiers(&usage, &self.price_tiers)
        })
    }
    pub(super) fn check_credentials(&self) -> Result<(), BackendError> {
        if let Some((_, expires)) = &self.chatgpt
            && *expires
                <= SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |time| time.as_secs())
        {
            return Err(BackendError::new(
                "OpenAI authorization expired — run /connect again",
            ));
        }
        Ok(())
    }
    /// A backend for `plan`.
    pub fn new(plan: TurnPlan) -> Result<Self, BackendError> {
        Ok(Self {
            client: http::client()?,
            endpoint: plan.endpoint,
            model: plan.model,
            max_tokens: plan.max_tokens,
            price: plan.price,
            price_tiers: Vec::new(),
            prompt_cache_key: plan.prompt_cache_key,
            chatgpt: None,
            capabilities: crate::ModelCapabilities {
                output_budget: Some(plan.max_tokens),
                ..Default::default()
            },
        })
    }

    /// Reuse the HTTP pool while fixing one request's resolved model parameters.
    pub fn with_model_parameters(
        mut self,
        model: String,
        max_tokens: u32,
        capabilities: crate::ModelCapabilities,
        price: Option<Price>,
        tiers: Vec<crate::PriceTier>,
    ) -> Self {
        self.model = model;
        self.max_tokens = max_tokens;
        self.capabilities = capabilities;
        self.capabilities.output_budget = Some(self.max_tokens);
        self.price = price;
        self.price_tiers = tiers;
        self
    }

    /// The protocol selected for this immutable request configuration.
    pub fn wire(&self) -> Wire {
        self.endpoint.wire
    }

    /// Select the protocol from the same resolution as the request model ID.
    pub fn with_wire(mut self, wire: Wire) -> Self {
        self.endpoint.wire = wire;
        self
    }

    /// Apply provider-specific context pricing tiers.
    pub fn with_price_tiers(mut self, tiers: Vec<crate::PriceTier>) -> Self {
        self.price_tiers = tiers;
        self
    }

    /// Attach explicit catalog metadata without inferring features from model names.
    pub fn with_capabilities(mut self, capabilities: crate::ModelCapabilities) -> Self {
        self.capabilities = capabilities;
        self.capabilities.output_budget = Some(self.max_tokens);
        self
    }

    /// Use the ChatGPT/Codex request profile rather than the public API profile.
    pub fn with_chatgpt(mut self, account_id: String, expires_at: u64) -> Self {
        self.chatgpt = Some((account_id, expires_at));
        self
    }

    /// Where requests go, for `--print-config`.
    pub fn base_url(&self) -> &str {
        &self.endpoint.base_url
    }

    /// The model this backend asks for.
    pub fn model(&self) -> &str {
        &self.model
    }
}

#[async_trait::async_trait]
impl AgentBackend for Backend {
    async fn model_client(&self) -> Result<Option<crate::ModelClient>, BackendError> {
        self.check_credentials()?;
        let adapter = std::sync::Arc::new(self.model_adapter()?);
        Ok(Some(crate::ModelClient {
            backend: self.clone(),
            adapter,
        }))
    }
    fn capabilities(&self) -> crate::ModelCapabilities {
        self.capabilities.clone()
    }
    fn label(&self) -> &str {
        self.endpoint.label
    }
    async fn run_turn(&self, request: TurnRequest) -> Result<AgentEventStream, BackendError> {
        // Compatibility display API; every request uses the native connection.
        let client = self
            .model_client()
            .await?
            .ok_or_else(|| BackendError::new("model connection unavailable"))?;
        let input_characters = request_characters(&request);
        let request = convert::request(&request, self.max_tokens)?;
        let kind = self.prepare_effect(rig_core::effect::EffectKind::Completion {
            request: std::sync::Arc::new(request),
            stream: true,
        })?;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let reply = crate::request::execute(
            client.clone(),
            kind,
            rig_core::serve::Dispatch::new(rig_core::effect::EffectId::from_raw(0), true),
            Some(tx),
        );
        let mut events = crate::events::adapt(client, reply.into_stream(), rx, input_characters);
        let mut opening = Vec::new();
        while let Some(event) = events.next().await {
            match event {
                crate::AgentEvent::Error(error) => return Err(BackendError::new(error)),
                event @ crate::AgentEvent::Status(_) => opening.push(event),
                event => {
                    opening.push(event);
                    return Ok(Box::pin(futures::stream::iter(opening).chain(events)));
                }
            }
        }
        Err(BackendError::new(
            "model stream ended without a complete response",
        ))
    }
    /// Ask the provider which models it offers.
    ///
    /// One attempt, no retries: this feeds a picker, so a slow or unreachable
    /// provider reports an error rather than leaving the picker loading forever.
    async fn models(&self) -> Result<Vec<String>, BackendError> {
        self.discover_models().await
    }
}

/// Characters of prompt and history, for the estimate fallback.
fn request_characters(request: &TurnRequest) -> usize {
    request.prompt.chars().count()
        + request
            .history
            .iter()
            .map(|message| message.characters())
            .sum::<usize>()
}

impl Backend {
    pub(super) fn completion_model(
        &self,
        repairs: &[crate::policy::Repair],
    ) -> Result<DynModel<Completion>, BackendError> {
        use rig_core::catalog::{Catalog, ModelFacts, ModelSpec};
        use rig_core::providers::registry::ProviderId;
        let transport = CompletionRoute::new(self.request_http(repairs), self.endpoint.url());
        // Bind Solaris metadata under a declared identity so every Rig dialect
        // consults the supplied facts, including aliased gateway model names.
        let id =
            ProviderId::declare("solaris").map_err(|error| BackendError::new(error.to_string()))?;
        let mut spec = ModelSpec::new(id, &self.model)
            .with_tools(self.capabilities.tool_calling != Support::Unsupported);
        spec.input.image = self.capabilities.image_input != Support::Unsupported;
        spec.context_window = self.capabilities.context_window;
        spec.max_input_tokens = self.capabilities.max_input;
        spec.max_output_tokens = self.capabilities.max_output;
        let facts = ModelFacts::new(spec).with_catalog(&Catalog::default());
        let model = match self.endpoint.wire {
            Wire::AnthropicMessages => {
                let config = self.anthropic_config();
                let mut wire = config.completion(&self.model).with_facts(facts);
                if self.capabilities.prompt_caching != Support::Unsupported {
                    wire = wire.with_prompt_caching();
                }
                Model::new(wire, transport)
                    .with_idle_timeout(Some(http::IDLE_TIMEOUT))
                    .erase()
            }
            wire_kind => {
                let config = self.openai_config();
                if wire_kind == Wire::OpenAiChat {
                    Model::new(config.chat(&self.model).with_facts(facts), transport)
                        .with_idle_timeout(Some(http::IDLE_TIMEOUT))
                        .erase()
                } else {
                    let mut wire =
                        rig_core::providers::openai::responses_api::wire::Responses::new(
                            config,
                            &self.model,
                        )
                        .with_facts(facts);
                    wire.strict_tools = false;
                    Model::new(wire, transport)
                        .with_idle_timeout(Some(http::IDLE_TIMEOUT))
                        .erase()
                }
            }
        };
        Ok(model)
    }
    pub(crate) fn anthropic_config(&self) -> rig_core::providers::anthropic::AnthropicConfig {
        rig_core::providers::anthropic::AnthropicConfig::new(
            self.endpoint.secret.clone().unwrap_or_default(),
        )
        .with_base_url(&self.endpoint.base_url)
    }

    pub(crate) fn openai_config(&self) -> rig_core::providers::openai::OpenAIConfig {
        use rig_core::providers::openai::{OpenAIConfig, wire};
        let dialect = if self.chatgpt.is_some() {
            &rig_core::providers::chatgpt::DIALECT
        } else {
            wire::by_name(self.endpoint.label).unwrap_or(&wire::OPENAI)
        };
        let mut config =
            OpenAIConfig::with_key(dialect, self.endpoint.secret.clone().unwrap_or_default())
                .with_base_url(&self.endpoint.base_url);
        config.instructions = None;
        if self.endpoint.secret.is_none() {
            config.auth = wire::Auth::OptionalBearer;
        }
        if let Some((account, _)) = &self.chatgpt {
            config.account_id = Some(account.clone());
            config.identity = Some(wire::CallerIdentity {
                originator: "solaris".into(),
                user_agent: concat!("solaris/", env!("CARGO_PKG_VERSION")).into(),
            });
        }
        config
    }

    pub(crate) fn describe_error(&self, report: &rig_core::error::ErrorReport) -> BackendError {
        let text = if let Some(status) = report
            .http_status
            .and_then(|code| reqwest::StatusCode::from_u16(code).ok())
        {
            http::describe(
                status,
                report.provider_response_body().unwrap_or(&report.message),
                self.endpoint.name,
                self.endpoint.env_keys,
            )
        } else {
            format!("could not reach {}: {}", self.endpoint.name, report.message)
        };
        if report.is_context_overflow() {
            BackendError::new(format!("context length exceeded: {text}"))
        } else {
            BackendError::new(text)
        }
    }
}
