//! Model facts come exclusively from the official catalog.
use rig_core::catalog::{Catalog, ModelFacts, ModelSpec};
use rig_core::providers::registry::ProviderId;
use serde::{Deserialize, Serialize};
use solaris_types::{ModelCapabilities, Support};
use std::{collections::BTreeMap, sync::Arc};

const MANUFACTURERS: &[&str] = &[
    "openai",
    "anthropic",
    "google",
    "deepseek",
    "xai",
    "mistral",
    "meta",
    "moonshotai",
    "zhipuai",
    "minimax",
];

/// Routing is independent of the catalog identity; aliases never change the endpoint.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSelection {
    pub provider: String,
    pub request_id: String,
    #[serde(default)]
    pub metadata: Option<String>,
    #[serde(default)]
    pub output_tokens: Option<u32>,
}

/// A named endpoint; credentials remain in AuthStore, never in model metadata.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderEndpoint {
    pub protocol: Protocol,
    pub base_url: String,
    #[serde(default)]
    pub credential: Option<String>,
}
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    OpenaiChat,
    OpenaiResponses,
    AnthropicMessages,
}
impl Protocol {
    pub(crate) fn wire(self) -> solaris_backend::Wire {
        match self {
            Self::OpenaiChat => solaris_backend::Wire::OpenAiChat,
            Self::OpenaiResponses => solaris_backend::Wire::OpenAiResponses,
            Self::AnthropicMessages => solaris_backend::Wire::AnthropicMessages,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub catalog: crate::catalog::ModelCatalog,
    selections: Arc<BTreeMap<String, ModelSelection>>,
    endpoints: Arc<BTreeMap<String, ProviderEndpoint>>,
}
impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            catalog: crate::catalog::ModelCatalog::default(),
            selections: Arc::new(BTreeMap::new()),
            endpoints: Arc::new(BTreeMap::new()),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ResolvedModel {
    pub provider_id: String,
    pub request_id: String,
    pub metadata: Option<String>,
    pub name: String,
    pub output_tokens: u32,
    pub capabilities: ModelCapabilities,
    pub reference_price: bool,
    #[serde(skip)]
    pub(crate) format: Option<rig_core::providers::registry::Format>,
    #[serde(serialize_with = "serialize_facts")]
    pub facts: ModelFacts,
}

fn serialize_facts<S: serde::Serializer>(
    facts: &ModelFacts,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    facts.spec().serialize(serializer)
}
impl ResolvedModel {
    pub fn without_pricing(mut self) -> Self {
        if let Some(spec) = self.facts.spec() {
            let mut spec = spec.clone();
            spec.pricing = None;
            self.facts = ModelFacts::new(spec).with_catalog(&Catalog::default());
        }
        self.capabilities.price_known = Some(false);
        self.reference_price = false;
        self
    }
}

impl ProviderConfig {
    /// New configuration: named selections and an official catalog override object.
    pub fn from_json(text: &str) -> Result<Self, String> {
        Self::parse(text, None)
    }
    /// Load cached upstream facts before checking explicit model references.
    pub fn from_json_with_cache(text: &str, directory: &std::path::Path) -> Result<Self, String> {
        Self::parse(text, Some(directory))
    }
    fn parse(text: &str, directory: Option<&std::path::Path>) -> Result<Self, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Config {
            #[serde(default)]
            models: BTreeMap<String, ModelSelection>,
            #[serde(default)]
            providers: BTreeMap<String, ProviderEndpoint>,
            #[serde(default = "empty_object")]
            overrides: serde_json::Value,
        }
        fn empty_object() -> serde_json::Value {
            serde_json::json!({})
        }
        if text.len() > 1024 * 1024 {
            return Err("model configuration exceeds 1 MiB".into());
        }
        let config: Config = serde_json::from_str(text).map_err(|e| e.to_string())?;
        for (name, model) in &config.models {
            if name.is_empty()
                || model.provider.is_empty()
                || model.request_id.is_empty()
                || model.output_tokens == Some(0)
                || (crate::provider_spec(&model.provider).is_none()
                    && !config.providers.contains_key(&model.provider))
            {
                return Err("model selections need a name, provider, request_id and a positive output budget".into());
            }
        }
        for (id, endpoint) in &config.providers {
            if id.is_empty() || crate::provider_spec(id).is_some() {
                return Err("custom endpoints need a unique non-built-in provider name".into());
            }
            let url =
                reqwest::Url::parse(&endpoint.base_url).map_err(|_| "invalid provider base_url")?;
            if !matches!(url.scheme(), "http" | "https")
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err("provider base_url must be an HTTP(S) URL without credentials, query or fragment".into());
            }
        }
        let overrides = serde_json::to_string(&config.overrides).map_err(|e| e.to_string())?;
        let base = Catalog::builtin();
        let patch = Catalog::from_overrides(&overrides, base).map_err(|e| e.to_string())?;
        let catalog = base.with_overrides(&patch);
        let result = Self {
            catalog: crate::catalog::ModelCatalog::new(catalog, overrides),
            selections: Arc::new(config.models),
            endpoints: Arc::new(config.providers),
        };
        if let Some(directory) = directory {
            result.catalog.load_cache(directory);
        }
        {
            let catalog = result
                .catalog
                .state
                .read()
                .unwrap_or_else(|p| p.into_inner());
            for selection in result.selections.values() {
                if let Some(reference) = &selection.metadata {
                    catalog.resolve(reference).map_err(|e| e.to_string())?;
                }
            }
        }
        Ok(result)
    }

    pub fn selection(&self, name: &str) -> Option<&ModelSelection> {
        self.selections.get(name)
    }
    pub fn endpoint(&self, name: &str) -> Option<&ProviderEndpoint> {
        self.endpoints.get(name)
    }

    pub fn resolve(&self, provider: &str, model: &str) -> ResolvedModel {
        let selection = self
            .selections
            .get(model)
            .filter(|s| s.provider == provider);
        let request_id = selection.map_or(model, |s| s.request_id.as_str());
        let vendor = if provider == "opencode" {
            "opencode-go"
        } else {
            provider
        };
        let catalog = self.catalog.state.read().unwrap_or_else(|p| p.into_inner());
        let explicit = selection.and_then(|s| s.metadata.as_deref());
        let direct = format!("{vendor}/{request_id}");
        // Missing models are ordinary for local servers. `resolve` builds typo
        // suggestions across the whole catalog; the hot path needs only lookup.
        let lookup = |reference: &str| {
            let (vendor, id) = reference.split_once('/')?;
            catalog.get(ProviderId::catalog(vendor)?, id)
        };
        let mut found = lookup(explicit.unwrap_or(&direct));
        if found.is_none()
            && explicit.is_none()
            && !catalog.iter().any(|s| s.provider.vendor() == vendor)
        {
            let mut matches = MANUFACTURERS
                .iter()
                .filter_map(|vendor| lookup(&format!("{vendor}/{request_id}")));
            let first = matches.next();
            if matches.next().is_none() {
                found = first;
            }
        }
        let metadata = found
            .as_ref()
            .map(|r| format!("{}/{}", r.spec.provider.vendor(), r.spec.id));
        let known = found.is_some();
        let mut spec = found.map_or_else(
            || {
                ModelSpec::new(
                    ProviderId::declare("solaris").expect("valid declared vendor"),
                    request_id,
                )
                .with_tools(true)
            },
            |r| r.spec.clone(),
        );
        let name = spec.display_name.clone();
        let reference_price = metadata
            .as_ref()
            .is_some_and(|s| !s.starts_with(&format!("{vendor}/")));
        let format = known.then(|| spec.format()).flatten();
        // Bind metadata to the wire's requested alias, under a declared identity.
        spec.provider = ProviderId::declare("solaris").expect("valid declared vendor");
        spec.id = request_id.into();
        let mut capabilities = capabilities(&spec, known);
        let output_tokens = crate::request::output_budget(
            &capabilities,
            selection.and_then(|s| s.output_tokens).unwrap_or(
                crate::DEFAULT_MAX_OUTPUT
                    .min(
                        capabilities
                            .context_window
                            .unwrap_or(crate::DEFAULT_CONTEXT_WINDOW)
                            / 4,
                    )
                    .max(1),
            ),
        );
        capabilities.output_budget = Some(output_tokens);
        ResolvedModel {
            provider_id: provider.into(),
            request_id: request_id.into(),
            metadata,
            name,
            output_tokens,
            capabilities,
            reference_price,
            format,
            facts: ModelFacts::new(spec).with_catalog(&Catalog::default()),
        }
    }
    /// Official catalog rows and explicitly configured aliases for the picker.
    pub fn models(&self, provider: &str) -> Vec<(String, String)> {
        let vendor = if provider == "opencode" {
            "opencode-go"
        } else {
            provider
        };
        let catalog = self.catalog.state.read().unwrap_or_else(|p| p.into_inner());
        let mut models: BTreeMap<String, String> = catalog
            .iter()
            .filter(|s| s.provider.vendor() == vendor && !s.deprecated && s.input.text)
            .map(|s| (s.id.clone(), s.display_name.clone()))
            .collect();
        for (name, selection) in self
            .selections
            .iter()
            .filter(|(_, s)| s.provider == provider)
        {
            models.insert(
                name.clone(),
                selection
                    .metadata
                    .clone()
                    .unwrap_or_else(|| selection.request_id.clone()),
            );
        }
        models.into_iter().collect()
    }
    pub(crate) fn wire(
        &self,
        provider: &str,
        model: &ResolvedModel,
        fallback: solaris_backend::Wire,
    ) -> solaris_backend::Wire {
        // Only this preset multiplexes protocol families. Custom endpoints and
        // ordinary gateways always honor their explicitly configured protocol.
        if provider != "opencode" || model.metadata.is_none() {
            return fallback;
        }
        match model.format {
            Some(rig_core::providers::registry::Format::Anthropic) => {
                solaris_backend::Wire::AnthropicMessages
            }
            Some(rig_core::providers::registry::Format::OpenAi) => {
                if model.facts.spec().is_some_and(|s| {
                    matches!(
                        s.compat.chat_tools,
                        Some(rig_core::catalog::ChatTools::Never)
                    )
                }) {
                    solaris_backend::Wire::OpenAiResponses
                } else {
                    solaris_backend::Wire::OpenAiChat
                }
            }
            _ => fallback,
        }
    }
}
fn capabilities(spec: &ModelSpec, known: bool) -> ModelCapabilities {
    fn support(value: bool, known: bool) -> Support {
        if !known {
            Support::Unknown
        } else if value {
            Support::Supported
        } else {
            Support::Unsupported
        }
    }
    ModelCapabilities {
        tool_calling: support(spec.tools, known),
        structured_output: support(spec.structured_output, known),
        image_input: support(spec.input.image, known),
        reasoning: support(spec.reasoning.supported(), known),
        prompt_caching: if spec.caching.retention.is_empty() {
            Support::Unknown
        } else {
            Support::Supported
        },
        temperature: match spec.sampling {
            Some(rig_core::catalog::Sampling::Any) => Support::Supported,
            Some(rig_core::catalog::Sampling::Never) => Support::Unsupported,
            _ => Support::Unknown,
        },
        context_window: spec.context_window,
        max_input: spec.max_input_tokens,
        max_output: spec.max_output_tokens,
        price_known: Some(spec.pricing.is_some()),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn picker_and_protocol_use_official_facts_instead_of_manual_model_records() {
        let config = ProviderConfig::default();
        let models = config.models("anthropic");
        assert!(models.iter().any(|(id, _)| id == "claude-sonnet-4-5"));
        assert!(
            !models.iter().any(|(id, _)| id == "claude-opus-4-1"),
            "deprecated rows are excluded"
        );
        let claude = config.resolve("opencode", "claude-haiku-5-5");
        assert_eq!(
            config.wire("opencode", &claude, solaris_backend::Wire::OpenAiChat),
            solaris_backend::Wire::AnthropicMessages
        );
        assert_eq!(
            config.wire("new-api", &claude, solaris_backend::Wire::OpenAiChat),
            solaris_backend::Wire::OpenAiChat,
            "gateway protocol is explicit"
        );
    }

    #[test]
    fn gateway_identity_preserves_request_id_and_uses_official_facts() {
        let catalog = ProviderConfig::default();
        let official = catalog.resolve("anthropic", "claude-sonnet-4-5");
        let gateway = catalog.resolve("new-api", "claude-sonnet-4-5");
        assert_eq!(gateway.request_id, "claude-sonnet-4-5");
        assert_eq!(gateway.metadata, official.metadata);
        assert_eq!(gateway.capabilities, official.capabilities);
        assert!(gateway.reference_price);
        assert!(!official.reference_price);
    }
    #[test]
    fn explicit_alias_keeps_routing_separate_from_metadata() {
        let catalog = ProviderConfig::from_json(r#"{"models":{"fast":{"provider":"new-api","request_id":"private-alias","metadata":"anthropic/claude-sonnet-4-5","output_tokens":4096}}}"#).expect("configuration");
        let model = catalog.resolve("new-api", "fast");
        assert_eq!(model.request_id, "private-alias");
        assert_eq!(model.output_tokens, 4096);
        assert_eq!(model.facts.spec().expect("facts").id, "private-alias");
        assert!(model.metadata.expect("metadata").starts_with("anthropic/"));
        assert_eq!(catalog.resolve("other", "fast").metadata, None);
    }
    #[test]
    fn unknown_and_wrong_vendor_do_not_invent_metadata() {
        let catalog = ProviderConfig::default();
        let unknown = catalog.resolve("local", "unlisted-model");
        assert_eq!(unknown.capabilities.tool_calling, Support::Unknown);
        assert_eq!(unknown.capabilities.context_window, None);
        assert_eq!(unknown.capabilities.price_known, Some(false));
        assert!(catalog.resolve("anthropic", "gpt-5").metadata.is_none());
        assert!(ProviderConfig::from_json(r#"{"models":{"x":{"provider":"new-api","request_id":"x","metadata":"anthropic/not-listed"}}}"#).is_err());
    }
    #[test]
    fn official_override_parser_updates_facts_and_limits() {
        let catalog = ProviderConfig::from_json(r#"{"overrides":{"anthropic":{"models":{"claude-sonnet-4-5":{"limit":{"context":32000,"output":2000}}}}}}"#).expect("override");
        let model = catalog.resolve("anthropic", "claude-sonnet-4-5");
        assert_eq!(model.capabilities.context_window, Some(32000));
        assert_eq!(model.output_tokens, 2000);
        assert_eq!(
            model.facts.spec().expect("facts").context_window,
            Some(32000)
        );
    }
}
