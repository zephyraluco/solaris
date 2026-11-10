//! One resolution supplies requests, budgets, capability checks and diagnostics.
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, OnceLock, RwLock};

use serde::{Deserialize, Serialize};
use solaris_backend::{Price, PriceTier};
use solaris_types::{ModelCapabilities, Support};

use super::schema::{CatalogModel, CatalogSnapshot, ParameterSources, valid_price};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCatalog {
    #[serde(default)]
    pub models: Vec<ModelOverride>,
    #[serde(skip)]
    pub(super) state: SharedCatalog,
}

#[derive(Debug, Clone)]
pub(super) struct SharedCatalog(pub Arc<RwLock<BTreeMap<String, CatalogSnapshot>>>);
impl Default for SharedCatalog {
    fn default() -> Self {
        static BUNDLED: OnceLock<CatalogSnapshot> = OnceLock::new();
        let snapshot = BUNDLED.get_or_init(|| {
            serde_json::from_str(include_str!("snapshot.json")).unwrap_or_default()
        });
        Self(Arc::new(RwLock::new(BTreeMap::from([(
            "snapshot:bundled".into(),
            snapshot.clone(),
        )]))))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogReference {
    pub provider: String,
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelOverride {
    pub provider: String,
    pub id: String,
    #[serde(default)]
    pub request_id: Option<String>,
    /// Requested reply budget, separately capped by model limits.
    #[serde(default)]
    pub output_tokens: Option<u32>,
    #[serde(default)]
    pub catalog: Option<CatalogReference>,
    #[serde(default)]
    pub capabilities: ModelCapabilities,
    #[serde(default)]
    pub price: Option<Price>,
    #[serde(default)]
    pub price_tiers: Vec<PriceTier>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResolvedModel {
    pub provider_id: String,
    pub id: String,
    pub request_id: String,
    pub catalog: Option<CatalogReference>,
    pub name: String,
    pub output_tokens: u32,
    pub capabilities: ModelCapabilities,
    pub price: Option<Price>,
    pub price_tiers: Vec<PriceTier>,
    pub sources: ParameterSources,
}

fn apply_capabilities(
    target: &mut ModelCapabilities,
    patch: &ModelCapabilities,
    source: &str,
    sources: &mut ParameterSources,
) {
    macro_rules! support {
        ($($field:ident),*) => { $(if patch.$field != Support::Unknown {
            target.$field = patch.$field;
            sources.insert(stringify!($field).into(), source.into());
        })* };
    }
    macro_rules! limit {
        ($($field:ident),*) => { $(if patch.$field.is_some() {
            target.$field = patch.$field;
            sources.insert(stringify!($field).into(), source.into());
        })* };
    }
    support!(
        tool_calling,
        structured_output,
        image_input,
        reasoning,
        streaming_usage,
        prompt_caching,
        temperature
    );
    limit!(context_window, max_input, max_output);
}

impl ModelCatalog {
    pub fn from_json(text: &str) -> Result<Self, String> {
        if text.len() > 1024 * 1024 {
            return Err("model overrides exceed 1 MiB".into());
        }
        let catalog: Self = serde_json::from_str(text).map_err(|error| error.to_string())?;
        let mut ids = BTreeSet::new();
        for entry in &catalog.models {
            if entry.provider.is_empty()
                || entry.id.is_empty()
                || entry.id.len() > 512
                || !ids.insert((&entry.provider, &entry.id))
            {
                return Err("empty or duplicate provider/model ID".into());
            }
            if entry
                .request_id
                .as_ref()
                .is_some_and(|id| id.is_empty() || id.len() > 512)
                || entry.catalog.as_ref().is_some_and(|reference| {
                    reference.provider.is_empty() || reference.id.is_empty()
                })
            {
                return Err("empty model reference or request ID".into());
            }
            if entry.output_tokens == Some(0)
                || entry.capabilities.context_window == Some(0)
                || entry.capabilities.max_output == Some(0)
                || entry.capabilities.max_input == Some(0)
            {
                return Err("model token limits must be positive".into());
            }
            if entry.price.is_some_and(|price| !valid_price(price))
                || entry
                    .price_tiers
                    .iter()
                    .any(|tier| !valid_price(tier.price))
            {
                return Err("model prices must be finite and nonnegative".into());
            }
            if !entry.price_tiers.is_empty() && entry.price.is_none() {
                return Err("price tiers require an explicit base price".into());
            }
        }
        Ok(catalog)
    }

    pub fn resolve(&self, provider: &str, model: &str) -> ResolvedModel {
        let patch = self
            .models
            .iter()
            .find(|entry| entry.provider == provider && entry.id == model);
        let mut reference = patch
            .and_then(|entry| entry.catalog.clone())
            .unwrap_or_else(|| CatalogReference {
                provider: provider.to_owned(),
                id: patch
                    .and_then(|entry| entry.request_id.clone())
                    .unwrap_or_else(|| model.into()),
            });
        let mut sources = ParameterSources::from([
            ("context_window".into(), "default".into()),
            ("max_output".into(), "default".into()),
            ("price".into(), "unknown".into()),
        ]);
        let mut capabilities = ModelCapabilities {
            context_window: Some(crate::DEFAULT_CONTEXT_WINDOW),
            max_output: Some(crate::DEFAULT_MAX_OUTPUT),
            ..ModelCapabilities::default()
        };
        // Both lookups share one read lock: a refresh cannot mix two generations.
        let layers = self
            .state
            .0
            .read()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut found = Self::lookup(&layers, &reference.provider, &reference.id);
        let provider_key = if provider == "opencode" {
            "opencode-go"
        } else {
            provider
        };
        let has_provider = layers
            .values()
            .any(|snapshot| snapshot.providers.contains_key(provider_key));
        let mut official_fallback = false;
        if found.is_none() && !has_provider && patch.is_none_or(|entry| entry.catalog.is_none()) {
            let candidates: Vec<_> = super::SNAPSHOT_PROVIDERS
                .iter()
                .filter_map(|official| {
                    Self::lookup(&layers, official, &reference.id).map(|entry| (*official, entry))
                })
                .collect();
            // Never choose arbitrarily between two manufacturers sharing an ID.
            if let [(official, entry)] = candidates.as_slice() {
                reference.provider = (*official).into();
                found = Some(entry.clone());
                official_fallback = true;
            }
        }
        let mut price = None;
        let mut tiers = Vec::new();
        let mut name = model.to_owned();
        if let Some((entry, source)) = &found {
            apply_capabilities(&mut capabilities, &entry.capabilities, source, &mut sources);
            name.clone_from(&entry.name);
        }
        // Capability inheritance across hosts never imports another host's bill.
        if let Some((entry, source)) = Self::lookup(
            &layers,
            provider,
            patch
                .and_then(|entry| entry.request_id.as_deref())
                .unwrap_or(model),
        ) {
            price = entry.price;
            tiers = entry.price_tiers;
            if price.is_some() {
                sources.insert("price".into(), source);
            }
        }
        if official_fallback && let Some((entry, source)) = &found {
            price = entry.price;
            tiers.clone_from(&entry.price_tiers);
            if price.is_some() {
                sources.insert(
                    "price".into(),
                    format!("official estimate:{}:{source}", reference.provider),
                );
            }
        }
        drop(layers);
        if let Some(patch) = patch {
            apply_capabilities(&mut capabilities, &patch.capabilities, "user", &mut sources);
            if let Some(value) = patch.price {
                price = Some(value);
                tiers.clone_from(&patch.price_tiers);
                sources.insert("price".into(), "user".into());
            }
        }
        capabilities.price_known = Some(price.is_some());
        let requested_output = patch.and_then(|entry| entry.output_tokens);
        let output_tokens = super::output_budget(
            &capabilities,
            requested_output.unwrap_or_else(|| {
                crate::DEFAULT_MAX_OUTPUT.min(
                    capabilities
                        .context_window
                        .unwrap_or(crate::DEFAULT_CONTEXT_WINDOW)
                        .saturating_div(4)
                        .max(1),
                )
            }),
        );
        capabilities.output_budget = Some(output_tokens);
        sources.insert(
            "output_tokens".into(),
            if requested_output.is_some() {
                "user"
            } else {
                "default"
            }
            .into(),
        );
        ResolvedModel {
            provider_id: provider.into(),
            id: model.into(),
            request_id: patch
                .and_then(|entry| entry.request_id.clone())
                .unwrap_or_else(|| model.into()),
            catalog: found.map(|_| reference),
            name,
            output_tokens,
            capabilities,
            price,
            price_tiers: tiers,
            sources,
        }
    }

    fn lookup(
        layers: &BTreeMap<String, CatalogSnapshot>,
        provider: &str,
        id: &str,
    ) -> Option<(CatalogModel, String)> {
        // Solaris's subscription endpoint corresponds to this upstream provider.
        let provider = if provider == "opencode" {
            "opencode-go"
        } else {
            provider
        };
        // Explicit order, independent of map key sorting.
        for key in ["cache:openrouter", "cache:models.dev", "snapshot:bundled"] {
            if let Some(entry) = layers
                .get(key)
                .and_then(|snapshot| snapshot.providers.get(provider))
                .and_then(|models| models.get(id))
            {
                return Some((entry.clone(), key.into()));
            }
        }
        None
    }

    pub fn capabilities(&self, provider: &str, model: &str) -> ModelCapabilities {
        self.resolve(provider, model).capabilities
    }
    pub fn price(&self, provider: &str, model: &str) -> Option<Price> {
        self.resolve(provider, model).price
    }
    pub fn model_ids(&self, provider: &str) -> Vec<String> {
        let provider_key = if provider == "opencode" {
            "opencode-go"
        } else {
            provider
        };
        let layers = self
            .state
            .0
            .read()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut ids: BTreeSet<String> = layers
            .values()
            .filter_map(|snapshot| snapshot.providers.get(provider_key))
            .flat_map(|models| models.keys().cloned())
            .collect();
        ids.extend(
            self.models
                .iter()
                .filter(|entry| entry.provider == provider)
                .map(|entry| entry.id.clone()),
        );
        ids.into_iter().collect()
    }

    /// Load only validated caches; offline startup always has the bundled snapshot.
    pub fn load_cache(&self, directory: &Path) {
        super::cache::load(self, directory);
    }
    /// Spawn a bounded periodic refresh; callers retain this shared catalog across switches.
    pub fn start_refresh(&self, directory: std::path::PathBuf) {
        super::cache::start(self.clone(), directory);
    }
}
