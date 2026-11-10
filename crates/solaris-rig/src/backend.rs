//! Rig-backed completion calls with Solaris endpoint, retry and billing policy.
use crate::{convert, http, transport::SolarisTransport};
use futures::StreamExt;
use rig_core::completion::{CompletionRequest, FinishReason};
use rig_core::driver::{DynModel, Model};
use rig_core::message::AssistantContent;
use rig_core::operation::Completion;
use rig_core::streaming::{Item, StreamEvent, Streamed};
use serde_json::Value;
use solaris_backend::{
    AgentBackend, AgentEvent, AgentEventStream, BackendError, Price, Support, TurnRequest, Usage,
    Wire,
};
use std::collections::VecDeque;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// This is the Codex catalog protocol version, not Solaris's package version.
// The service gates models by client version; 0.1.0 returns an empty catalog.
const CODEX_CATALOG_VERSION: &str = "0.162.1";

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
fn session_id() -> &'static str {
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
pub struct RigBackend {
    client: reqwest::Client,
    endpoint: Endpoint,
    model: String,
    max_tokens: u32,
    price: Option<Price>,
    price_tiers: Vec<solaris_backend::PriceTier>,
    prompt_cache_key: Option<&'static str>,
    chatgpt: Option<(String, u64)>,
    capabilities: solaris_backend::ModelCapabilities,
}

impl RigBackend {
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
            capabilities: solaris_backend::ModelCapabilities {
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
        capabilities: solaris_backend::ModelCapabilities,
        price: Option<Price>,
        tiers: Vec<solaris_backend::PriceTier>,
    ) -> Self {
        self.model = model;
        self.max_tokens = max_tokens;
        self.capabilities = capabilities;
        self.capabilities.output_budget = Some(self.max_tokens);
        self.price = price;
        self.price_tiers = tiers;
        self
    }

    /// Apply provider-specific context pricing tiers.
    pub fn with_price_tiers(mut self, tiers: Vec<solaris_backend::PriceTier>) -> Self {
        self.price_tiers = tiers;
        self
    }

    /// Attach explicit catalog metadata without inferring features from model names.
    pub fn with_capabilities(mut self, capabilities: solaris_backend::ModelCapabilities) -> Self {
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

    /// Attach the credential and the headers the wire wants.
    ///
    /// A key travels the way the protocol carrying it does: `x-api-key` on the
    /// Messages API, a bearer token on the OpenAI ones — opencode's Go gateway
    /// included, whose `/v1/messages` takes `x-api-key` while its
    /// OpenAI-compatible paths take a bearer token.
    fn authorize(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let request = if let Some((account, _)) = &self.chatgpt {
            request
                .header("ChatGPT-Account-Id", account)
                .header("originator", "solaris")
        } else {
            request
        };
        let request = match self.endpoint.wire {
            Wire::AnthropicMessages => request.header("anthropic-version", "2023-06-01"),
            Wire::OpenAiChat | Wire::OpenAiResponses => request,
        };

        let request = match &self.endpoint.secret {
            Some(secret) if self.endpoint.wire.is_openai() => request.bearer_auth(secret),
            Some(secret) => request.header("x-api-key", secret),
            None => request,
        };

        // A platform that routes by conversation is told which one this is, so
        // the turn lands on the plan it belongs to rather than on whichever
        // balance the gateway would otherwise guess at.
        match self.endpoint.session_header {
            Some(header) => request.header(header, session_id()),
            None => request,
        }
    }

    /// A status line for an attempt that is about to be retried.
    fn retrying(&self, wait: Duration) -> AgentEvent {
        AgentEvent::Status(format!(
            "{} is busy — retrying in {:.1}s",
            self.endpoint.name,
            wait.as_secs_f32()
        ))
    }

    /// A transport failure in the user's terms.
    fn unreachable(&self, error: &reqwest::Error) -> BackendError {
        BackendError::new(format!(
            "could not reach {}: {}",
            self.endpoint.name,
            http::describe_transport(error)
        ))
    }

    /// Why a successful response carried no model list.
    fn not_a_model_list(&self, url: &str, body: &str) -> String {
        let name = self.endpoint.name;
        if body.trim_start().starts_with('<') {
            return format!("{name} answered {url} with a web page, not a model list");
        }

        let reason = http::provider_message(body)
            .map(|message| format!(": {message}"))
            .unwrap_or_default();
        format!("{name} did not answer with a model list at {url}{reason}")
    }
}

#[async_trait::async_trait]
impl AgentBackend for RigBackend {
    fn capabilities(&self) -> solaris_backend::ModelCapabilities {
        self.capabilities.clone()
    }
    fn label(&self) -> &str {
        self.endpoint.label
    }
    async fn run_turn(&self, request: TurnRequest) -> Result<AgentEventStream, BackendError> {
        if self.capabilities.tool_calling == Support::Unsupported && !request.tools.is_empty() {
            return Err(BackendError::new(
                "this model does not support tool calling; choose another model or disable tools",
            ));
        }
        if self.capabilities.image_input == Support::Unsupported
            && request.history.iter().any(|message| {
                message.content.iter().any(|part| match part {
                    solaris_backend::Content::Image { .. } => true,
                    solaris_backend::Content::ToolResult { result } => {
                        !result.metadata.images.is_empty()
                    }
                    _ => false,
                })
            })
        {
            return Err(BackendError::new("this model does not support image input"));
        }
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
        let mut converted = convert::request(&request, self.max_tokens)?;
        let mut params = serde_json::Map::new();
        if self.endpoint.wire == Wire::OpenAiResponses {
            params.insert("store".into(), Value::Bool(false));
        }
        if let Some(key) = self.prompt_cache_key
            && self.capabilities.prompt_caching != Support::Unsupported
        {
            params.insert("prompt_cache_key".into(), Value::String(key.into()));
        }
        if !params.is_empty() {
            converted = converted.additional_params(Value::Object(params));
        }
        let model = self.rig_model()?;
        let stream = model
            .stream(converted.clone())
            .map_err(|error| self.rig_error(&error))?;
        let mut engine = Engine {
            backend: self.clone(),
            model,
            request: converted,
            stream: Some(stream),
            pending: VecDeque::new(),
            retries: 0,
            visible: false,
            done: false,
            input_characters: request_characters(&request),
            output_characters: 0,
        };
        let mut opening = VecDeque::new();
        loop {
            match engine.next().await {
                Some(AgentEvent::Error(error)) => return Err(BackendError::new(error)),
                Some(event @ AgentEvent::Status(_)) => opening.push_back(event),
                Some(event) => {
                    opening.push_back(event);
                    break;
                }
                None => return Err(BackendError::new("provider returned no completion")),
            }
        }
        opening.append(&mut engine.pending);
        engine.pending = opening;
        Ok(Box::pin(futures::stream::unfold(
            engine,
            |mut engine| async move { engine.next().await.map(|event| (event, engine)) },
        )))
    }
    /// Ask the provider which models it offers.
    ///
    /// One attempt, no retries: this feeds a picker, so a slow or unreachable
    /// provider reports an error rather than leaving the picker loading forever.
    async fn models(&self) -> Result<Vec<String>, BackendError> {
        let url = if self.chatgpt.is_some() {
            format!(
                "{}?client_version={}",
                self.endpoint.models_url(),
                CODEX_CATALOG_VERSION
            )
        } else {
            self.endpoint.models_url()
        };
        let response = self
            .authorize(self.client.get(&url))
            .header("accept", "application/json")
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|error| self.unreachable(&error))?;

        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|error| self.unreachable(&error))?;

        if !status.is_success() {
            return Err(BackendError::new(http::describe(
                status,
                &body,
                self.endpoint.name,
                self.endpoint.env_keys,
            )));
        }

        // A 200 that is not a model list is a failure, not an empty provider: a
        // base URL that stops short of a gateway's API gets its web front end
        // back, which answers 200 with a page, and reading that as "no models"
        // would hide a wrong URL behind a plausible-looking answer.
        let models = if self.chatgpt.is_some() {
            parse_codex_models(&body)
        } else {
            parse_models(&body)
        };
        let Some(models) = models else {
            return Err(BackendError::new(self.not_a_model_list(&url, &body)));
        };

        Ok(models)
    }
}
fn parse_codex_models(body: &str) -> Option<Vec<String>> {
    #[derive(serde::Deserialize)]
    struct Catalog {
        models: Vec<Model>,
    }
    #[derive(serde::Deserialize)]
    struct Model {
        slug: String,
        #[serde(default)]
        visibility: Option<String>,
        #[serde(default)]
        priority: i64,
    }
    let mut catalog: Catalog = serde_json::from_str(body).ok()?;
    if catalog
        .models
        .iter()
        .any(|model| model.slug.trim().is_empty())
    {
        return None;
    }
    catalog.models.sort_by_key(|model| model.priority);
    Some(
        catalog
            .models
            .into_iter()
            .filter(|model| {
                model
                    .visibility
                    .as_deref()
                    .is_none_or(|visibility| visibility == "list")
            })
            .map(|model| model.slug)
            .collect(),
    )
}

/// Model ids out of a `{"data": [{"id": …}]}` list.
///
/// Both wires answer with this shape. `None` means the body was not a model
/// list at all — a proxy's HTML page, an error wrapped in a 200 — which the
/// caller reports instead of passing off as a provider with nothing to offer.
/// An empty `data` array is a list: the provider answered, and its answer is
/// that this credential can reach no models.
fn parse_models(body: &str) -> Option<Vec<String>> {
    let value: Value = serde_json::from_str(body).ok()?;
    let entries = value["data"].as_array()?;

    Some(
        entries
            .iter()
            .filter_map(|entry| entry["id"].as_str())
            .map(str::to_owned)
            .collect(),
    )
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

impl RigBackend {
    fn rig_model(&self) -> Result<DynModel<Completion>, BackendError> {
        use rig_core::catalog::{Catalog, ModelFacts, ModelSpec};
        use rig_core::providers::registry::ProviderId;
        use rig_core::providers::{
            anthropic::AnthropicConfig,
            openai::{OpenAIConfig, wire},
        };
        let transport = SolarisTransport::new(
            self.client.clone(),
            self.endpoint.url(),
            self.endpoint.session_header,
            session_id(),
        );
        let provider = if self.endpoint.wire == Wire::AnthropicMessages {
            "anthropic"
        } else if self.chatgpt.is_some() {
            "chatgpt"
        } else {
            "openai"
        };
        let id = ProviderId::catalog(provider)
            .ok_or_else(|| BackendError::new("Rig provider unavailable"))?;
        let mut spec = ModelSpec::new(id, &self.model)
            .with_tools(self.capabilities.tool_calling != Support::Unsupported);
        spec.input.image = self.capabilities.image_input != Support::Unsupported;
        spec.context_window = self.capabilities.context_window;
        spec.max_input_tokens = self.capabilities.max_input;
        spec.max_output_tokens = self.capabilities.max_output;
        let facts = ModelFacts::new(spec).with_catalog(&Catalog::default());
        let key = self.endpoint.secret.clone().unwrap_or_default();
        let model = match self.endpoint.wire {
            Wire::AnthropicMessages => {
                let config = AnthropicConfig::new(key).with_base_url(&self.endpoint.base_url);
                let mut wire = config.completion(&self.model).with_facts(facts);
                if self.capabilities.prompt_caching != Support::Unsupported {
                    wire = wire.with_prompt_caching();
                }
                Model::new(wire, transport)
                    .with_idle_timeout(Some(http::IDLE_TIMEOUT))
                    .erase()
            }
            wire_kind => {
                let dialect = if self.chatgpt.is_some() {
                    &rig_core::providers::chatgpt::DIALECT
                } else {
                    wire::by_name(self.endpoint.label).unwrap_or(&wire::OPENAI)
                };
                let mut config =
                    OpenAIConfig::with_key(dialect, key).with_base_url(&self.endpoint.base_url);
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
                if wire_kind == Wire::OpenAiChat {
                    Model::new(config.chat(&self.model).with_facts(facts), transport)
                        .with_idle_timeout(Some(http::IDLE_TIMEOUT))
                        .erase()
                } else {
                    Model::new(
                        rig_core::providers::openai::responses_api::wire::Responses::new(
                            config,
                            &self.model,
                        )
                        .with_facts(facts),
                        transport,
                    )
                    .with_idle_timeout(Some(http::IDLE_TIMEOUT))
                    .erase()
                }
            }
        };
        Ok(model)
    }
    fn rig_error(&self, error: &rig_core::error::ProviderError) -> BackendError {
        let report = error.report();
        let text = if let Some(status) = report
            .http_status
            .and_then(|code| reqwest::StatusCode::from_u16(code).ok())
        {
            http::describe(
                status,
                error.provider_response_body().unwrap_or(&report.message),
                self.endpoint.name,
                self.endpoint.env_keys,
            )
        } else {
            format!("could not reach {}: {}", self.endpoint.name, report.message)
        };
        if error.report().is_context_overflow() {
            BackendError::new(format!("context length exceeded: {text}"))
        } else {
            BackendError::new(text)
        }
    }
}

struct Engine {
    backend: RigBackend,
    model: DynModel<Completion>,
    request: CompletionRequest,
    stream: Option<Streamed<Completion>>,
    pending: VecDeque<AgentEvent>,
    retries: usize,
    visible: bool,
    done: bool,
    input_characters: usize,
    output_characters: usize,
}
impl Engine {
    async fn next(&mut self) -> Option<AgentEvent> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Some(event);
            }
            if self.done {
                return None;
            }
            let stream = self.stream.as_mut()?;
            match stream.next().await {
                Some(Ok(Item::Event(event))) => {
                    let event = match event {
                        StreamEvent::Text { text, .. } => {
                            self.output_characters += text.chars().count();
                            Some(AgentEvent::TextDelta(text))
                        }
                        StreamEvent::Reasoning { text, .. } => {
                            Some(AgentEvent::ThinkingDelta(text))
                        }
                        StreamEvent::End {
                            content: AssistantContent::ToolCall(call),
                            ..
                        } => {
                            let arguments = if let Some(invalid) = &call.function.invalid_arguments
                            {
                                Value::String(invalid.clone())
                            } else {
                                call.function.arguments_value()
                            };
                            Some(AgentEvent::ToolCall(solaris_backend::ToolCall::new(
                                call.id.wire().into_owned(),
                                call.function.name.as_str(),
                                arguments,
                            )))
                        }
                        _ => None,
                    };
                    if let Some(event) = event {
                        self.visible = true;
                        return Some(event);
                    }
                }
                Some(Ok(Item::Unknown(_))) => {}
                Some(Err(error)) => {
                    let retryable = error
                        .report()
                        .http_status
                        .and_then(|status| reqwest::StatusCode::from_u16(status).ok())
                        .map_or_else(|| error.is_retryable(), http::is_retryable);
                    if !self.visible && retryable && self.retries < http::max_retries() {
                        let wait = error
                            .report()
                            .retry_after(SystemTime::now())
                            .unwrap_or_else(|| http::backoff(self.retries, None))
                            .min(Duration::from_secs(30));
                        self.retries += 1;
                        tokio::time::sleep(wait).await;
                        match self.model.stream(self.request.clone()) {
                            Ok(stream) => {
                                self.stream = Some(stream);
                                return Some(self.backend.retrying(wait));
                            }
                            Err(error) => {
                                self.done = true;
                                return Some(AgentEvent::Error(
                                    self.backend.rig_error(&error).to_string(),
                                ));
                            }
                        }
                    }
                    self.done = true;
                    return Some(AgentEvent::Error(
                        self.backend.rig_error(&error).to_string(),
                    ));
                }
                None => {
                    self.done = true;
                    let response = match self.stream.take()?.finish().await {
                        Ok(response) => response,
                        Err(error) => {
                            return Some(AgentEvent::Error(
                                self.backend.rig_error(&error).to_string(),
                            ));
                        }
                    };
                    if let Some(error) = &response.error {
                        return Some(AgentEvent::Error(error.clone()));
                    }
                    if let Some(error) = &response.aborted {
                        return Some(AgentEvent::Error(error.clone()));
                    }
                    let measured = response.usage.input_tokens.is_some()
                        || response.usage.output_tokens.is_some();
                    let usage = if measured {
                        convert::usage(response.usage)
                    } else {
                        Usage::estimate(self.input_characters, self.output_characters)
                    };
                    let cost_usd = self.backend.price.map_or(0.0, |price| {
                        price.cost_with_tiers(&usage, &self.backend.price_tiers)
                    });
                    let complete = AgentEvent::TurnComplete { usage, cost_usd };
                    if matches!(response.finish_reason(), Some(FinishReason::Length)) {
                        self.pending.push_back(complete);
                        return Some(AgentEvent::OutputTruncated);
                    }
                    return Some(complete);
                }
            }
        }
    }
}
