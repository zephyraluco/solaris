//! Upstream wire formats never escape this module. Missing rates remain unknown.
use std::collections::BTreeMap;

use serde_json::Value;
use solaris_backend::{Price, PriceTier};
use solaris_types::{ModelCapabilities, Support};

use super::schema::{CatalogModel, CatalogSnapshot, valid_price};

pub const MODELS_DEV_URL: &str = "https://models.dev/experimental/v2.0/api.json";
pub const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/models";

fn support(value: &Value) -> Support {
    match value.as_bool() {
        Some(true) => Support::Supported,
        Some(false) => Support::Unsupported,
        None => Support::Unknown,
    }
}
fn positive(value: &Value) -> Option<u32> {
    value
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n > 0)
}
fn number(value: &Value) -> Option<f64> {
    value.as_f64().filter(|n| n.is_finite() && *n >= 0.0)
}
fn price(input: f64, output: f64, read: f64, write: f64) -> Option<Price> {
    let price = Price::per_million(input, output, read, write);
    valid_price(price).then_some(price)
}
fn cache_write(value: &Value) -> Option<f64> {
    number(value).or_else(|| number(&value["5m"]))
}
fn dev_price(value: &Value) -> Option<Price> {
    let input = &value["text"]["input"];
    let output = &value["text"]["output"];
    if input.get("unit").is_some_and(|v| v != "token")
        || output.get("unit").is_some_and(|v| v != "token")
    {
        return None;
    }
    price(
        number(&input["price"])?,
        number(&output["price"])?,
        number(&input["cache"]["read"]).unwrap_or(0.0),
        cache_write(&input["cache"]["write"]).unwrap_or(0.0),
    )
}
fn dev_tiers(cost: &Value, base: Option<Price>) -> Vec<PriceTier> {
    let Some(base) = base else { return Vec::new() };
    let mut tiers = BTreeMap::new();
    for (kind, field) in [("input", "input"), ("output", "output")] {
        let Some(overrides) = cost["text"][kind]["overrides"].as_array() else {
            continue;
        };
        for value in overrides {
            // Region, service and time-dependent prices cannot be inferred from token usage.
            let Some(when) = value["when"].as_object() else {
                continue;
            };
            if when.len() != 1 {
                continue;
            }
            let Some(threshold) = positive(&value["when"]["context"]["gt"]) else {
                continue;
            };
            let rates = tiers.entry(threshold).or_insert(base);
            if let Some(rate) = number(&value["price"]) {
                if field == "input" {
                    rates.input = rate;
                } else {
                    rates.output = rate;
                }
            }
            if kind == "input" {
                if let Some(rate) = number(&value["cache"]["read"]) {
                    rates.cache_read = rate;
                }
                if let Some(rate) = cache_write(&value["cache"]["write"]) {
                    rates.cache_write = rate;
                }
            }
        }
    }
    tiers
        .into_iter()
        .map(|(input_tokens_above, price)| PriceTier {
            input_tokens_above,
            price,
        })
        .collect()
}

pub fn parse_models_dev(text: &str) -> Result<CatalogSnapshot, String> {
    if text.len() > super::cache::MAX_BYTES {
        return Err("models.dev response is too large".into());
    }
    let root: Value = serde_json::from_str(text).map_err(|error| error.to_string())?;
    let providers = root.as_object().ok_or("expected a provider map")?;
    let mut result = CatalogSnapshot::default();
    for (provider, data) in providers {
        let Some(models) = data["models"].as_object() else {
            continue;
        };
        let mut entries = BTreeMap::new();
        for (id, model) in models {
            if model["type"].as_str().is_some_and(|kind| kind != "chat") {
                continue;
            }
            let output = model["modalities"]["output"].as_array();
            if !output.is_some_and(|values| values.iter().any(|v| v == "text")) {
                continue;
            }
            let images =
                model["modalities"]["input"]
                    .as_array()
                    .map_or(Support::Unknown, |values| {
                        if values.iter().any(|v| {
                            v.as_str()
                                .is_some_and(|s| s == "image" || s.starts_with("image/"))
                        }) {
                            Support::Supported
                        } else {
                            Support::Unsupported
                        }
                    });
            let caps = &model["capabilities"];
            let capabilities = ModelCapabilities {
                tool_calling: support(&caps["tools"]["supported"]),
                structured_output: support(&caps["structured_output"]),
                reasoning: support(&caps["reasoning"]["supported"]),
                image_input: images,
                temperature: support(&caps["temperature"]),
                streaming_usage: support(&caps["stream_usage"]),
                prompt_caching: if caps["cache"]["implicit"] == true
                    || caps["cache"]["explicit"].is_object()
                {
                    Support::Supported
                } else {
                    support(&caps["cache"])
                },
                context_window: positive(&model["limit"]["context"]),
                max_input: positive(&model["limit"]["input"]),
                max_output: positive(&model["limit"]["output"]),
                ..ModelCapabilities::default()
            };
            let price = dev_price(&model["cost"]);
            entries.insert(
                id.clone(),
                CatalogModel {
                    name: model["name"].as_str().unwrap_or(id).to_owned(),
                    capabilities,
                    price,
                    price_tiers: dev_tiers(&model["cost"], price),
                },
            );
        }
        if !entries.is_empty() {
            result.providers.insert(provider.clone(), entries);
        }
    }
    if result.providers.is_empty() {
        return Err("models.dev returned no chat models".into());
    }
    Ok(result)
}

pub fn parse_openrouter(text: &str) -> Result<CatalogSnapshot, String> {
    if text.len() > super::cache::MAX_BYTES {
        return Err("OpenRouter response is too large".into());
    }
    let root: Value = serde_json::from_str(text).map_err(|error| error.to_string())?;
    let mut entries = BTreeMap::new();
    for model in root["data"]
        .as_array()
        .ok_or("expected an OpenRouter model list")?
    {
        let Some(id) = model["id"].as_str().filter(|id| !id.is_empty()) else {
            continue;
        };
        let Some(outputs) = model["architecture"]["output_modalities"].as_array() else {
            continue;
        };
        if !outputs.iter().any(|v| v == "text") {
            continue;
        }
        let parameters = model["supported_parameters"].as_array();
        let has = |name: &str| {
            parameters.map_or(Support::Unknown, |list| {
                if list.iter().any(|v| v == name) {
                    Support::Supported
                } else {
                    Support::Unsupported
                }
            })
        };
        let images =
            model["architecture"]["input_modalities"]
                .as_array()
                .map_or(Support::Unknown, |list| {
                    if list.iter().any(|v| v == "image") {
                        Support::Supported
                    } else {
                        Support::Unsupported
                    }
                });
        let rate = |field: &str| {
            model["pricing"][field]
                .as_str()
                .and_then(|s| s.parse::<f64>().ok())
                .filter(|n| n.is_finite() && *n >= 0.0)
                .map(|n| n * 1_000_000.0)
        };
        let price = rate("prompt")
            .zip(rate("completion"))
            .and_then(|(input, output)| {
                price(
                    input,
                    output,
                    rate("input_cache_read").unwrap_or(0.0),
                    rate("input_cache_write").unwrap_or(0.0),
                )
            });
        entries.insert(
            id.to_owned(),
            CatalogModel {
                name: model["name"].as_str().unwrap_or(id).to_owned(),
                capabilities: ModelCapabilities {
                    tool_calling: has("tools"),
                    reasoning: has("reasoning"),
                    structured_output: has("structured_outputs"),
                    temperature: has("temperature"),
                    image_input: images,
                    context_window: positive(&model["top_provider"]["context_length"])
                        .or_else(|| positive(&model["context_length"])),
                    max_output: positive(&model["top_provider"]["max_completion_tokens"]),
                    ..ModelCapabilities::default()
                },
                price,
                price_tiers: Vec::new(),
            },
        );
    }
    if entries.is_empty() {
        return Err("OpenRouter returned no chat models".into());
    }
    Ok(CatalogSnapshot {
        providers: BTreeMap::from([("openrouter".into(), entries)]),
    })
}
