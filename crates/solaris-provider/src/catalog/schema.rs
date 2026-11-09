//! The compact, provider-scoped representation shared by all upstream adapters.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use solaris_backend::{Price, PriceTier};
use solaris_types::ModelCapabilities;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CatalogSnapshot {
    pub providers: BTreeMap<String, BTreeMap<String, CatalogModel>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogModel {
    pub name: String,
    pub capabilities: ModelCapabilities,
    pub price: Option<Price>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub price_tiers: Vec<PriceTier>,
}

/// Field-level provenance is retained after merging user overrides.
pub type ParameterSources = BTreeMap<String, String>;

pub(super) fn valid_price(price: Price) -> bool {
    [
        price.input,
        price.output,
        price.cache_read,
        price.cache_write,
    ]
    .iter()
    .all(|rate| rate.is_finite() && *rate >= 0.0)
}

/// Model manufacturers included in the offline distribution, excluding resellers.
pub const SNAPSHOT_PROVIDERS: &[&str] = &[
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

impl CatalogSnapshot {
    /// Keep the bundled catalog focused; downloaded caches can still cover all hosts.
    pub fn retain_snapshot_providers(&mut self) {
        self.providers
            .retain(|provider, _| SNAPSHOT_PROVIDERS.contains(&provider.as_str()));
    }

    pub fn from_json(text: &str) -> Result<Self, String> {
        if text.len() > super::cache::MAX_BYTES {
            return Err("model snapshot is too large".into());
        }
        let snapshot: Self = serde_json::from_str(text).map_err(|error| error.to_string())?;
        if snapshot.providers.is_empty() || snapshot.providers.values().all(BTreeMap::is_empty) {
            return Err("model snapshot is empty".into());
        }
        for models in snapshot.providers.values() {
            for (id, model) in models {
                if id.is_empty()
                    || id.len() > 512
                    || model.capabilities.context_window == Some(0)
                    || model.capabilities.max_input == Some(0)
                    || model.capabilities.max_output == Some(0)
                    || model.price.is_some_and(|price| !valid_price(price))
                    || model
                        .price_tiers
                        .iter()
                        .any(|tier| !valid_price(tier.price))
                {
                    return Err(format!("invalid model parameters: {id}"));
                }
            }
        }
        Ok(snapshot)
    }
}
