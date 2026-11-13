//! Model facts come exclusively from the official catalog.
use rig_core::catalog::{Catalog, ModelFacts, ModelSpec};
use rig_core::providers::registry::ProviderId;
use serde::{Deserialize, Serialize};
use solaris_types::{ModelCapabilities, Support};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, RwLock},
};

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

#[derive(Debug, Clone)]
pub struct ModelCatalog {
    pub(super) state: Arc<RwLock<Catalog>>,
    pub(super) overrides: Arc<String>,
    selections: Arc<BTreeMap<String, ModelSelection>>,
}
impl Default for ModelCatalog {
    fn default() -> Self {
        Self {
            state: Arc::new(RwLock::new(Catalog::builtin().clone())),
            overrides: Arc::new("{}".into()),
            selections: Arc::new(BTreeMap::new()),
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
    pub facts: ModelFacts,
}

impl ModelCatalog {
    /// New configuration: named selections and an official catalog override object.
    pub fn from_json(text: &str) -> Result<Self, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Config {
            #[serde(default)]
            models: BTreeMap<String, ModelSelection>,
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
            {
                return Err("model selections need a name, provider, request_id and a positive output budget".into());
            }
        }
        let overrides = serde_json::to_string(&config.overrides).map_err(|e| e.to_string())?;
        let base = Catalog::builtin();
        let patch = Catalog::from_overrides(&overrides, base).map_err(|e| e.to_string())?;
        let catalog = base.with_overrides(&patch);
        for selection in config.models.values() {
            if let Some(reference) = &selection.metadata {
                catalog.resolve(reference).map_err(|e| e.to_string())?;
            }
        }
        Ok(Self {
            state: Arc::new(RwLock::new(catalog)),
            overrides: Arc::new(overrides),
            selections: Arc::new(config.models),
        })
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
        let catalog = self.state.read().unwrap_or_else(|p| p.into_inner());
        let explicit = selection.and_then(|s| s.metadata.as_deref());
        let direct = format!("{vendor}/{request_id}");
        let mut found = catalog.resolve(explicit.unwrap_or(&direct)).ok();
        if found.is_none()
            && explicit.is_none()
            && !catalog.iter().any(|s| s.provider.vendor() == vendor)
        {
            let mut matches = MANUFACTURERS
                .iter()
                .filter_map(|vendor| catalog.resolve(&format!("{vendor}/{request_id}")).ok());
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
        // Bind metadata to the wire's requested alias, under a declared identity.
        spec.provider = ProviderId::declare("solaris").expect("valid declared vendor");
        spec.id = request_id.into();
        let mut capabilities = capabilities(&spec, known);
        let output_tokens = super::super::discovery::output_budget(
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
            facts: ModelFacts::new(spec).with_catalog(&Catalog::default()),
        }
    }
    pub fn capabilities(&self, provider: &str, model: &str) -> ModelCapabilities {
        self.resolve(provider, model).capabilities
    }
    pub fn model_ids(&self, provider: &str) -> Vec<String> {
        let vendor = if provider == "opencode" {
            "opencode-go"
        } else {
            provider
        };
        let catalog = self.state.read().unwrap_or_else(|p| p.into_inner());
        let mut ids: Vec<_> = catalog
            .iter()
            .filter(|s| s.provider.vendor() == vendor && !s.deprecated)
            .map(|s| s.id.clone())
            .collect();
        ids.extend(
            self.selections
                .iter()
                .filter(|(_, s)| s.provider == provider)
                .map(|(name, _)| name.clone()),
        );
        ids.sort();
        ids.dedup();
        ids
    }
    pub fn load_cache(&self, directory: &Path) {
        super::cache::load(self, directory);
    }
    pub fn start_refresh(&self, directory: std::path::PathBuf) {
        super::cache::start(self.clone(), directory);
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
        temperature: Support::Unknown,
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
    fn gateway_identity_preserves_request_id_and_uses_official_facts() {
        let catalog = ModelCatalog::default();
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
        let catalog = ModelCatalog::from_json(r#"{"models":{"fast":{"provider":"new-api","request_id":"private-alias","metadata":"anthropic/claude-sonnet-4-5","output_tokens":4096}}}"#).expect("configuration");
        let model = catalog.resolve("new-api", "fast");
        assert_eq!(model.request_id, "private-alias");
        assert_eq!(model.output_tokens, 4096);
        assert_eq!(model.facts.spec().expect("facts").id, "private-alias");
        assert!(model.metadata.expect("metadata").starts_with("anthropic/"));
        assert_eq!(catalog.resolve("other", "fast").metadata, None);
    }
    #[test]
    fn unknown_and_wrong_vendor_do_not_invent_metadata() {
        let catalog = ModelCatalog::default();
        let unknown = catalog.resolve("local", "unlisted-model");
        assert_eq!(unknown.capabilities.tool_calling, Support::Unknown);
        assert_eq!(unknown.capabilities.context_window, None);
        assert_eq!(unknown.capabilities.price_known, Some(false));
        assert!(catalog.resolve("anthropic", "gpt-5").metadata.is_none());
        assert!(ModelCatalog::from_json(r#"{"models":{"x":{"provider":"new-api","request_id":"x","metadata":"anthropic/not-listed"}}}"#).is_err());
    }
    #[test]
    fn official_override_parser_updates_facts_and_limits() {
        let catalog = ModelCatalog::from_json(r#"{"overrides":{"anthropic":{"models":{"claude-sonnet-4-5":{"limit":{"context":32000,"output":2000}}}}}}"#).expect("override");
        let model = catalog.resolve("anthropic", "claude-sonnet-4-5");
        assert_eq!(model.capabilities.context_window, Some(32000));
        assert_eq!(model.output_tokens, 2000);
        assert_eq!(
            model.facts.spec().expect("facts").context_window,
            Some(32000)
        );
    }
}
