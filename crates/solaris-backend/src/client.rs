//! Rig-backed completion calls with Solaris endpoint, retry and billing policy.
use crate::{AgentBackend, BackendError, Support, Wire};
use crate::{http, transport::CompletionRoute};
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
    pub name: String,
    /// Short label shown in the status bar — the platform id.
    pub label: String,
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
/// endpoint, which model, how large a reply to ask for, and whether a prompt-cache
/// key should ride along. Official model facts are attached separately.
#[derive(Debug, Clone)]
pub struct TurnPlan {
    /// Where the turn goes and how it authenticates.
    pub endpoint: Endpoint,
    /// Model id to send, exactly as the caller wants it.
    pub model: String,
    /// Largest reply to ask for.
    pub max_tokens: u32,
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
    pub(super) facts: rig_core::catalog::ModelFacts,
    pub(super) prompt_cache_key: Option<&'static str>,
    pub(super) chatgpt: Option<(String, u64)>,
    pub(super) capabilities: crate::ModelCapabilities,
}

impl Backend {
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
        use rig_core::catalog::{Catalog, ModelFacts, ModelSpec};
        use rig_core::providers::registry::ProviderId;
        let mut spec = ModelSpec::new(
            ProviderId::declare("solaris").expect("valid declared vendor"),
            &plan.model,
        )
        .with_tools(true);
        spec.input.image = true;
        spec.max_output_tokens = Some(plan.max_tokens);
        Ok(Self {
            client: http::client()?,
            endpoint: plan.endpoint,
            model: plan.model,
            max_tokens: plan.max_tokens,
            facts: ModelFacts::new(spec).with_catalog(&Catalog::default()),
            prompt_cache_key: plan.prompt_cache_key,
            chatgpt: None,
            capabilities: crate::ModelCapabilities {
                output_budget: Some(plan.max_tokens),
                ..Default::default()
            },
        })
    }

    /// Bind the official catalog facts to an immutable request configuration.
    pub fn with_model_facts(
        mut self,
        model: String,
        max_tokens: u32,
        facts: rig_core::catalog::ModelFacts,
        capabilities: crate::ModelCapabilities,
    ) -> Self {
        self.model = model;
        self.max_tokens = max_tokens;
        self.capabilities = capabilities;
        self.facts = facts;
        self
    }

    /// The protocol selected for this immutable request configuration.
    pub fn wire(&self) -> Wire {
        self.endpoint.wire
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
    async fn model_client(self: std::sync::Arc<Self>) -> Result<crate::ModelClient, BackendError> {
        crate::ModelClient::native((*self).clone())
    }
    fn capabilities(&self) -> crate::ModelCapabilities {
        self.capabilities.clone()
    }
    fn label(&self) -> &str {
        &self.endpoint.label
    }
    /// Ask the provider which models it offers.
    ///
    /// One attempt, no retries: this feeds a picker, so a slow or unreachable
    /// provider reports an error rather than leaving the picker loading forever.
    async fn models(&self) -> Result<Vec<String>, BackendError> {
        self.discover_models().await
    }
}

impl Backend {
    pub(super) fn completion_model(
        &self,
        repairs: &[crate::policy::Repair],
    ) -> Result<DynModel<Completion>, BackendError> {
        let transport = CompletionRoute::new(self.request_http(repairs), self.endpoint.url());
        let facts = self.facts.clone();
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
            wire::by_name(&self.endpoint.label).unwrap_or(&wire::OPENAI)
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
                &self.endpoint.name,
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
