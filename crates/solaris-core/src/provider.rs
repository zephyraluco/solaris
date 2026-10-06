//! The provider catalogue the `/connect` wizard offers.
//!
//! Mirrors claurst's `claurst-api` provider list in miniature: each entry says
//! how it authenticates and which models it offers, so the wizard's steps and
//! the follow-up model picker are both driven from one table.

use serde::{Deserialize, Serialize};

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

/// One selectable provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// Models offered once the provider is connected: id plus description.
    pub models: &'static [(&'static str, &'static str)],
}

/// Every provider the wizard lists, in display order.
pub const PROVIDERS: &[ProviderSpec] = &[
    ProviderSpec {
        id: "anthropic",
        name: "Anthropic",
        description: "Claude models — API key",
        auth: AuthKind::ApiKey,
        badge: None,
        models: &[
            ("claude-sonnet-4-5", "balanced"),
            ("claude-opus-4-1", "most capable"),
            ("claude-haiku-4-5", "fastest"),
        ],
    },
    ProviderSpec {
        id: "claude-subscription",
        name: "Claude subscription",
        description: "Sign in with a Pro or Max plan",
        auth: AuthKind::DeviceCode,
        badge: None,
        models: &[
            ("claude-sonnet-4-5", "balanced"),
            ("claude-opus-4-1", "most capable"),
        ],
    },
    ProviderSpec {
        id: "openai",
        name: "OpenAI",
        description: "GPT models — API key",
        auth: AuthKind::ApiKey,
        badge: None,
        models: &[("gpt-5", "flagship"), ("gpt-5-mini", "cheap and fast")],
    },
    ProviderSpec {
        id: "google",
        name: "Google",
        description: "Gemini models — API key",
        auth: AuthKind::ApiKey,
        badge: None,
        models: &[
            ("gemini-2.5-pro", "long context"),
            ("gemini-2.5-flash", "fast"),
        ],
    },
    ProviderSpec {
        id: "groq",
        name: "Groq",
        description: "Open models on fast hardware — API key",
        auth: AuthKind::ApiKey,
        badge: Some("FREE"),
        models: &[
            ("llama-3.3-70b", "open weights"),
            ("qwen-3-32b", "open weights"),
        ],
    },
    ProviderSpec {
        id: "openrouter",
        name: "OpenRouter",
        description: "One key for many upstreams",
        auth: AuthKind::ApiKey,
        badge: None,
        models: &[("auto", "the router picks a model")],
    },
    ProviderSpec {
        id: "local",
        name: "Local runtime",
        description: "Ollama or llama.cpp on this machine",
        auth: AuthKind::Local,
        badge: Some("LOCAL"),
        models: &[("llama3.2", "local"), ("qwen2.5-coder", "local")],
    },
    ProviderSpec {
        id: "custom",
        name: "Custom endpoint",
        description: "Any OpenAI-compatible endpoint",
        auth: AuthKind::ApiKeyWithUrl,
        badge: None,
        models: &[],
    },
];

/// Look up a provider by id.
pub fn provider(id: &str) -> Option<&'static ProviderSpec> {
    PROVIDERS.iter().find(|spec| spec.id == id)
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
            for (model, description) in spec.models {
                assert!(!model.is_empty());
                assert!(!description.is_empty(), "{model} has no description");
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
}
