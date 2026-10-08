//! The provider catalogue: what each platform offers, and how to reach it.
//!
//! Mirrors claurst's `claurst-api` provider list in miniature: each entry says
//! how it authenticates, which wire protocol it speaks, where its endpoint
//! lives, which environment variables carry its credentials, and which models
//! it offers. The wizard's steps, the model picker and the price table are all
//! driven from this one table, and [`crate::choose_backend`] turns one of them
//! into a backend.

use serde::{Deserialize, Serialize};
use solaris_backend::Wire;
use solaris_core::Price;

/// How a provider authenticates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthKind {
    /// Nothing to collect — a runtime on this machine.
    Local,
    /// A single API key.
    ApiKey,
    /// An OpenAI-compatible endpoint: base URL plus an optional key.
    ApiKeyWithUrl,
    /// Device-code / browser OAuth.
    DeviceCode,
}

/// One model a provider offers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelSpec {
    /// Identifier sent to the provider.
    pub id: &'static str,
    /// One-line description shown after the ` · ` separator in the picker.
    pub description: &'static str,
    /// Context window in tokens, which sizes the footer's context gauge.
    pub context_window: u32,
    /// Largest reply the provider will produce for this model.
    pub max_output: u32,
    /// List price, or `None` when it varies per request — a router's `auto`.
    pub price: Option<Price>,
}

/// One selectable provider.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProviderSpec {
    /// Stable identifier used as the credential key.
    pub id: &'static str,
    /// Display name.
    pub name: &'static str,
    /// One-line description shown after the ` · ` separator.
    pub description: &'static str,
    /// Which wizard step follows the picker.
    pub auth: AuthKind,
    /// Short right-aligned badge (`FREE`, `LOCAL`).
    pub badge: Option<&'static str>,
    /// Protocol used once a credential exists.
    pub wire: Wire,
    /// Fixed endpoint, or `None` when the endpoint comes from the credential.
    pub base_url: Option<&'static str>,
    /// Environment variables checked for a key, most specific first.
    pub env_keys: &'static [&'static str],
    /// Environment variables that override `base_url`.
    pub base_url_envs: &'static [&'static str],
    /// Models offered once the provider is connected.
    pub models: &'static [ModelSpec],
}

/// Context window assumed for a model the catalogue does not list.
pub const DEFAULT_CONTEXT_WINDOW: u32 = 128_000;

/// Largest reply asked of a model the catalogue does not list.
pub const DEFAULT_MAX_OUTPUT: u32 = 8_192;

/// Terser alias for the price table below — USD per million tokens.
const fn usd(input: f64, output: f64, cache_read: f64, cache_write: f64) -> Price {
    Price::per_million(input, output, cache_read, cache_write)
}

/// Every provider the wizard lists, in display order.
pub const PROVIDERS: &[ProviderSpec] = &[
    ProviderSpec {
        id: "anthropic",
        name: "Anthropic",
        description: "Claude models — API key",
        auth: AuthKind::ApiKey,
        badge: None,
        wire: Wire::AnthropicMessages,
        base_url: Some("https://api.anthropic.com"),
        env_keys: &["ANTHROPIC_API_KEY"],
        base_url_envs: &["ANTHROPIC_BASE_URL"],
        models: &[
            ModelSpec {
                id: "claude-sonnet-4-5",
                description: "balanced",
                context_window: 200_000,
                max_output: 64_000,
                price: Some(usd(3.00, 15.00, 0.30, 3.75)),
            },
            ModelSpec {
                id: "claude-opus-4-1",
                description: "most capable",
                context_window: 200_000,
                max_output: 32_000,
                price: Some(usd(15.00, 75.00, 1.50, 18.75)),
            },
            ModelSpec {
                id: "claude-haiku-4-5",
                description: "fastest",
                context_window: 200_000,
                max_output: 64_000,
                price: Some(usd(1.00, 5.00, 0.10, 1.25)),
            },
        ],
    },
    ProviderSpec {
        id: "claude-subscription",
        name: "Claude subscription",
        description: "Sign in with a Pro or Max plan",
        auth: AuthKind::DeviceCode,
        badge: None,
        wire: Wire::AnthropicMessages,
        base_url: Some("https://api.anthropic.com"),
        env_keys: &[],
        base_url_envs: &[],
        models: &[
            ModelSpec {
                id: "claude-sonnet-4-5",
                description: "balanced",
                context_window: 200_000,
                max_output: 64_000,
                price: Some(usd(3.00, 15.00, 0.30, 3.75)),
            },
            ModelSpec {
                id: "claude-opus-4-1",
                description: "most capable",
                context_window: 200_000,
                max_output: 32_000,
                price: Some(usd(15.00, 75.00, 1.50, 18.75)),
            },
        ],
    },
    ProviderSpec {
        id: "openai",
        name: "OpenAI",
        description: "GPT models — API key",
        auth: AuthKind::ApiKey,
        badge: None,
        wire: Wire::OpenAiResponses,
        base_url: Some("https://api.openai.com/v1"),
        env_keys: &["OPENAI_API_KEY"],
        base_url_envs: &["OPENAI_BASE_URL"],
        models: &[
            ModelSpec {
                id: "gpt-5",
                description: "flagship",
                context_window: 400_000,
                max_output: 128_000,
                price: Some(usd(1.25, 10.00, 0.125, 0.00)),
            },
            ModelSpec {
                id: "gpt-5-mini",
                description: "cheap and fast",
                context_window: 400_000,
                max_output: 128_000,
                price: Some(usd(0.25, 2.00, 0.025, 0.00)),
            },
        ],
    },
    ProviderSpec {
        id: "google",
        name: "Google",
        description: "Gemini models — API key",
        auth: AuthKind::ApiKey,
        badge: None,
        // Gemini's own protocol is not worth a third client: Google publishes
        // an OpenAI-compatible endpoint that speaks this one.
        wire: Wire::OpenAiChat,
        base_url: Some("https://generativelanguage.googleapis.com/v1beta/openai"),
        env_keys: &["GOOGLE_API_KEY", "GEMINI_API_KEY"],
        base_url_envs: &[],
        models: &[
            ModelSpec {
                id: "gemini-2.5-pro",
                description: "long context",
                context_window: 1_048_576,
                max_output: 65_536,
                price: Some(usd(1.25, 10.00, 0.31, 0.00)),
            },
            ModelSpec {
                id: "gemini-2.5-flash",
                description: "fast",
                context_window: 1_048_576,
                max_output: 65_536,
                price: Some(usd(0.30, 2.50, 0.075, 0.00)),
            },
        ],
    },
    ProviderSpec {
        id: "new-api",
        name: "New API",
        description: "Self-hosted OpenAI-compatible gateway — URL + API key",
        auth: AuthKind::ApiKeyWithUrl,
        badge: None,
        wire: Wire::OpenAiChat,
        // A gateway lives wherever it was deployed, so the endpoint comes from
        // `/connect` rather than from this table.
        base_url: None,
        env_keys: &[],
        base_url_envs: &[],
        // Its model list is whatever the operator configured upstreams for, so
        // there is nothing honest to ship here; `/model <name>` sends one
        // as written.
        models: &[],
    },
    ProviderSpec {
        id: "openrouter",
        name: "OpenRouter",
        description: "One key for many upstreams",
        auth: AuthKind::ApiKey,
        badge: None,
        wire: Wire::OpenAiChat,
        base_url: Some("https://openrouter.ai/api/v1"),
        env_keys: &["OPENROUTER_API_KEY"],
        base_url_envs: &["OPENROUTER_BASE_URL"],
        models: &[ModelSpec {
            id: "auto",
            description: "the router picks a model",
            context_window: 128_000,
            max_output: 8_192,
            // Billed at whichever upstream the router chose, so there is no
            // list price to report.
            price: None,
        }],
    },
    ProviderSpec {
        id: "local",
        name: "Local runtime",
        description: "Ollama or llama.cpp on this machine",
        auth: AuthKind::Local,
        badge: Some("LOCAL"),
        wire: Wire::OpenAiChat,
        base_url: Some("http://localhost:11434/v1"),
        env_keys: &[],
        base_url_envs: &["SOLARIS_LOCAL_BASE_URL"],
        models: &[
            ModelSpec {
                id: "llama3.2",
                description: "local",
                context_window: 131_072,
                max_output: 8_192,
                price: Some(Price::FREE),
            },
            ModelSpec {
                id: "qwen2.5-coder",
                description: "local",
                context_window: 131_072,
                max_output: 8_192,
                price: Some(Price::FREE),
            },
        ],
    },
    ProviderSpec {
        id: "custom",
        name: "Custom endpoint",
        description: "Any OpenAI-compatible endpoint",
        auth: AuthKind::ApiKeyWithUrl,
        badge: None,
        wire: Wire::OpenAiChat,
        // The endpoint is whatever `/connect` collected.
        base_url: None,
        env_keys: &[],
        base_url_envs: &[],
        models: &[],
    },
];

/// Look up a provider by id.
pub fn provider(id: &str) -> Option<&'static ProviderSpec> {
    PROVIDERS.iter().find(|spec| spec.id == id)
}

/// Every model the catalogue lists, in table order.
pub fn models() -> impl Iterator<Item = &'static ModelSpec> {
    PROVIDERS.iter().flat_map(|spec| spec.models.iter())
}

/// The catalogue entry for `model`.
///
/// Providers date-suffix their model ids (`claude-sonnet-4-5-20250929`), so an
/// exact miss falls back to the longest catalogue id that prefixes it.
pub fn model_spec(model: &str) -> Option<&'static ModelSpec> {
    models()
        .find(|spec| spec.id.eq_ignore_ascii_case(model))
        .or_else(|| {
            let lowered = model.to_ascii_lowercase();
            models()
                .filter(|spec| lowered.starts_with(spec.id))
                .max_by_key(|spec| spec.id.len())
        })
}

/// List price for `model`.
///
/// `None` means the price is unknown — either the model is not in the
/// catalogue, or it is one whose price varies per request — so callers report
/// no cost rather than a wrong one.
pub fn price_for(model: &str) -> Option<Price> {
    model_spec(model).and_then(|spec| spec.price)
}

/// Largest reply to ask `model` for.
pub fn max_output_for(model: &str) -> u32 {
    model_spec(model).map_or(DEFAULT_MAX_OUTPUT, |spec| spec.max_output)
}

/// Context window of `model`, which sizes the footer's context gauge.
pub fn context_window_for(model: &str) -> u32 {
    model_spec(model).map_or(DEFAULT_CONTEXT_WINDOW, |spec| spec.context_window)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_lookup_works() {
        let mut ids: Vec<_> = PROVIDERS.iter().map(|spec| spec.id).collect();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count, "duplicate provider id");

        for spec in PROVIDERS {
            assert_eq!(provider(spec.id).map(|found| found.name), Some(spec.name));
        }
        assert!(provider("nope").is_none());
    }

    #[test]
    fn every_provider_describes_itself() {
        for spec in PROVIDERS {
            assert!(!spec.name.is_empty());
            assert!(
                !spec.description.is_empty(),
                "{} has no description",
                spec.id
            );
            for model in spec.models {
                assert!(!model.id.is_empty());
                assert!(
                    !model.description.is_empty(),
                    "{} has no description",
                    model.id
                );
            }
        }
    }

    #[test]
    fn the_catalog_exercises_every_auth_kind() {
        // The wizard has a step per auth kind; the shipped table must reach all
        // of them or those steps would be dead code.
        for kind in [
            AuthKind::Local,
            AuthKind::ApiKey,
            AuthKind::ApiKeyWithUrl,
            AuthKind::DeviceCode,
        ] {
            assert!(
                PROVIDERS.iter().any(|spec| spec.auth == kind),
                "no provider uses {kind:?}"
            );
        }
    }

    #[test]
    fn every_provider_can_reach_an_endpoint() {
        for spec in PROVIDERS {
            // A custom endpoint is the one provider that collects its URL, so
            // it is the only one allowed to have no default.
            assert_eq!(
                spec.base_url.is_none(),
                spec.auth == AuthKind::ApiKeyWithUrl,
                "{} has the wrong endpoint story",
                spec.id
            );
        }
    }

    #[test]
    fn every_wire_is_spoken_by_some_provider() {
        for wire in [
            Wire::AnthropicMessages,
            Wire::OpenAiChat,
            Wire::OpenAiResponses,
        ] {
            assert!(
                PROVIDERS.iter().any(|spec| spec.wire == wire),
                "{wire:?} is implemented but no provider speaks it"
            );
        }
    }

    #[test]
    fn only_openai_itself_leaves_the_compatibility_floor() {
        assert_eq!(
            provider("openai").expect("a known provider").wire,
            Wire::OpenAiResponses
        );

        // `/responses` is not implemented as widely as chat completions, so
        // every other compatible upstream stays on the floor.
        for id in ["new-api", "openrouter", "google", "local", "custom"] {
            assert_eq!(
                provider(id).expect("a known provider").wire,
                Wire::OpenAiChat,
                "{id} should speak chat completions"
            );
        }
    }

    #[test]
    fn a_gateway_collects_its_own_endpoint() {
        let gateway = provider("new-api").expect("a known provider");

        // A self-hosted gateway has no address in this table, so it takes the
        // wizard's URL step the way `custom` does.
        assert_eq!(gateway.auth, AuthKind::ApiKeyWithUrl);
        assert!(gateway.base_url.is_none());
        // Nothing honest can be listed: the upstreams behind it are the
        // operator's choice.
        assert!(gateway.models.is_empty());
        // The wire is the compatibility floor, which is what a gateway speaks.
        assert_eq!(gateway.wire, Wire::OpenAiChat);
    }

    #[test]
    fn the_two_openai_wires_are_openai() {
        assert!(Wire::OpenAiChat.is_openai());
        assert!(Wire::OpenAiResponses.is_openai());
        assert!(!Wire::AnthropicMessages.is_openai());
    }

    #[test]
    fn only_the_key_providers_read_the_environment() {
        for spec in PROVIDERS {
            assert_eq!(
                !spec.env_keys.is_empty(),
                spec.auth == AuthKind::ApiKey,
                "{} reads the environment unexpectedly",
                spec.id
            );
        }
    }

    #[test]
    fn model_lookup_accepts_date_suffixed_ids() {
        let spec = model_spec("claude-sonnet-4-5-20250929").expect("prefix match");
        assert_eq!(spec.id, "claude-sonnet-4-5");

        assert_eq!(model_spec("gpt-5").expect("exact match").id, "gpt-5");

        // A model nobody has heard of falls back to the defaults.
        assert!(model_spec("some-new-model").is_none());
        assert_eq!(price_for("some-new-model"), None);
        assert_eq!(max_output_for("some-new-model"), DEFAULT_MAX_OUTPUT);
        assert_eq!(context_window_for("some-new-model"), DEFAULT_CONTEXT_WINDOW);
    }

    #[test]
    fn only_the_routed_model_has_no_list_price() {
        let unpriced: Vec<_> = models()
            .filter(|spec| spec.price.is_none())
            .map(|spec| spec.id)
            .collect();
        assert_eq!(unpriced, vec!["auto"]);
    }

    #[test]
    fn local_models_are_free_and_hosted_ones_are_not() {
        for spec in PROVIDERS {
            for model in spec.models {
                let Some(price) = model.price else {
                    continue;
                };
                match spec.auth {
                    AuthKind::Local => assert_eq!(price, Price::FREE, "{}", model.id),
                    _ => assert!(price.input > 0.0, "{} bills nothing", model.id),
                }
            }
        }
    }
}
