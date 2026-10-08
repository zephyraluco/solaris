//! Credentials and the platform catalogue.
//!
//! [`AuthStore`] holds the credentials collected by `/connect` and remembers
//! which provider is active. Reading and writing the file is deliberately left
//! to the application, so this crate stays pure and testable — the store
//! serializes to and from JSON, and `solaris` decides where that JSON lives.
//!
//! [`providers`] is the platform table: which providers exist, how each
//! authenticates, where its endpoint lives, which models it offers and what they
//! cost. [`choose_backend`] turns a credential plus a model name into a backend,
//! so nothing above this crate has to know the difference between one platform
//! and another.

pub mod choose;
pub mod providers;

pub use choose::{
    BackendChoice, BackendOptions, CredentialSource, Environment, build_backend, choose_backend,
    empty_environment, process_environment,
};
pub use solaris_backend::Wire;

pub use providers::{
    AuthKind, DEFAULT_CONTEXT_WINDOW, DEFAULT_MAX_OUTPUT, ModelSpec, PROVIDERS, ProviderSpec,
    context_window_for, max_output_for, model_spec, models, price_for, provider_spec,
};

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A stored credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Credential {
    /// A provider API key.
    ApiKey { key: String },
    /// An OpenAI-compatible endpoint and its optional key.
    Endpoint { base_url: String, api_key: String },
    /// An OAuth access token.
    Token { token: String },
}

impl Credential {
    /// Short label for the kind of credential.
    pub fn kind(&self) -> &'static str {
        match self {
            Credential::ApiKey { .. } => "api key",
            Credential::Endpoint { .. } => "endpoint",
            Credential::Token { .. } => "oauth token",
        }
    }

    /// Redacted form, safe to print.
    pub fn masked(&self) -> String {
        match self {
            Credential::ApiKey { key } => mask_secret(key),
            Credential::Endpoint { base_url, .. } => base_url.clone(),
            Credential::Token { token } => mask_secret(token),
        }
    }

    /// The secret itself, when the credential carries one.
    pub fn secret(&self) -> Option<&str> {
        match self {
            Credential::ApiKey { key } => Some(key),
            Credential::Endpoint { api_key, .. } => Some(api_key),
            Credential::Token { token } => Some(token),
        }
    }
}

/// Mask all but the last four characters, as the wizard does on screen.
pub fn mask_secret(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= 4 {
        return value.to_string();
    }
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{}{}", "\u{2022}".repeat(chars.len() - 4), tail)
}

/// The provider requests go to, and the model chosen for it.
///
/// The two travel together because a model is chosen *for* a provider: a model
/// id means nothing to a provider that never offered it, so switching providers
/// must not carry the choice across.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Active {
    /// Provider id.
    pub provider: String,
    /// Model identifier, empty until one is chosen.
    #[serde(default)]
    pub model: String,
}

/// Read the active selection, accepting the bare provider id older files hold.
fn active_from_json<'de, D>(deserializer: D) -> Result<Option<Active>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Shape {
        Provider(String),
        Selection(Active),
    }

    Ok(match Option::<Shape>::deserialize(deserializer)? {
        None => None,
        Some(Shape::Provider(provider)) => Some(Active {
            provider,
            model: String::new(),
        }),
        Some(Shape::Selection(active)) => Some(active),
    })
}

/// Credentials for this installation plus the active provider and model.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthStore {
    #[serde(default)]
    entries: BTreeMap<String, Credential>,
    #[serde(default, deserialize_with = "active_from_json")]
    active: Option<Active>,
}

impl AuthStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Store (or replace) a provider's credential.
    pub fn store(&mut self, provider_id: impl Into<String>, credential: Credential) {
        self.entries.insert(provider_id.into(), credential);
    }

    /// A provider's credential.
    pub fn credential(&self, provider_id: &str) -> Option<&Credential> {
        self.entries.get(provider_id)
    }

    /// Whether a provider has a credential.
    pub fn is_connected(&self, provider_id: &str) -> bool {
        self.entries.contains_key(provider_id)
    }

    /// Drop a provider's credential, clearing the active provider if it was it.
    pub fn remove(&mut self, provider_id: &str) -> bool {
        let removed = self.entries.remove(provider_id).is_some();
        if removed && self.active_provider() == Some(provider_id) {
            self.active = None;
        }
        removed
    }

    /// Mark a provider as the one requests go to.
    ///
    /// A model chosen for another provider is dropped with it; re-activating the
    /// same provider keeps the model it was used with.
    pub fn activate(&mut self, provider_id: impl Into<String>) {
        let provider = provider_id.into();
        let unchanged = self
            .active
            .as_ref()
            .is_some_and(|active| active.provider == provider);
        if !unchanged {
            self.active = Some(Active {
                provider,
                model: String::new(),
            });
        }
    }

    /// The active provider id, when one is set.
    pub fn active_provider(&self) -> Option<&str> {
        self.active.as_ref().map(|active| active.provider.as_str())
    }

    /// Remember the model the active provider should be asked for.
    pub fn remember_model(&mut self, model: impl Into<String>) {
        if let Some(active) = self.active.as_mut() {
            active.model = model.into();
        }
    }

    /// The model chosen for the active provider, when one has been.
    pub fn active_model(&self) -> Option<&str> {
        let model = self.active.as_ref()?.model.as_str();
        (!model.is_empty()).then_some(model)
    }

    /// The active provider's credential.
    pub fn active_credential(&self) -> Option<&Credential> {
        self.active_provider().and_then(|id| self.credential(id))
    }

    /// Whether nothing has been connected yet.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Number of stored credentials.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Every connected provider id, in stable order.
    pub fn connected(&self) -> Vec<&str> {
        self.entries.keys().map(String::as_str).collect()
    }

    /// Serialize for persistence.
    pub fn to_json(&self) -> Result<String, AuthError> {
        serde_json::to_string_pretty(self).map_err(|error| AuthError::Encode(error.to_string()))
    }

    /// Parse a previously serialized store.
    pub fn from_json(text: &str) -> Result<Self, AuthError> {
        serde_json::from_str(text).map_err(|error| AuthError::Decode(error.to_string()))
    }
}

/// Failures raised while (de)serializing the store.
#[derive(Debug, Error)]
pub enum AuthError {
    #[error("could not encode credentials: {0}")]
    Encode(String),
    #[error("could not read credentials: {0}")]
    Decode(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masking_keeps_only_the_last_four_characters() {
        assert_eq!(mask_secret("sk-1234567890"), "\u{2022}".repeat(9) + "7890");
        assert_eq!(mask_secret("abc"), "abc");
        assert_eq!(mask_secret(""), "");
        assert_eq!(mask_secret("ééééé"), "\u{2022}éééé");
    }

    #[test]
    fn storing_and_activating_a_provider() {
        let mut store = AuthStore::new();
        assert!(store.is_empty());
        assert!(store.active_provider().is_none());

        store.store("anthropic", Credential::ApiKey { key: "sk-1".into() });
        assert!(store.is_connected("anthropic"));
        assert!(!store.is_connected("openai"));
        assert_eq!(store.len(), 1);

        store.activate("anthropic");
        assert_eq!(store.active_provider(), Some("anthropic"));
        assert_eq!(
            store.active_credential().and_then(Credential::secret),
            Some("sk-1")
        );
    }

    #[test]
    fn removing_the_active_provider_clears_the_active_slot() {
        let mut store = AuthStore::new();
        store.store("groq", Credential::ApiKey { key: "g".into() });
        store.activate("groq");

        assert!(store.remove("groq"));
        assert_eq!(store.active_provider(), None);
        assert!(!store.remove("groq"));
    }

    #[test]
    fn the_model_is_kept_for_the_provider_it_was_chosen_for() {
        let mut store = AuthStore::new();
        store.store("opencode", Credential::ApiKey { key: "k".into() });
        store.activate("opencode");
        assert_eq!(store.active_model(), None);

        store.remember_model("minimax-m3");
        assert_eq!(store.active_model(), Some("minimax-m3"));

        // Re-activating the same provider keeps its model.
        store.activate("opencode");
        assert_eq!(store.active_model(), Some("minimax-m3"));

        // Another provider would not know that model, so it does not inherit it.
        store.store("anthropic", Credential::ApiKey { key: "a".into() });
        store.activate("anthropic");
        assert_eq!(store.active_model(), None);
    }

    #[test]
    fn a_file_holding_only_a_provider_id_still_loads() {
        // Older builds wrote `"active": "anthropic"`; the model simply is not
        // remembered yet, and the credentials must survive the upgrade.
        let json =
            r#"{"entries":{"anthropic":{"kind":"api_key","key":"sk-1"}},"active":"anthropic"}"#;

        let store = AuthStore::from_json(json).expect("decode");
        assert_eq!(store.active_provider(), Some("anthropic"));
        assert_eq!(store.active_model(), None);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn credentials_round_trip_through_json() {
        let mut store = AuthStore::new();
        store.store("anthropic", Credential::ApiKey { key: "sk-a".into() });
        store.store(
            "custom",
            Credential::Endpoint {
                base_url: "https://example.test/v1".into(),
                api_key: "k".into(),
            },
        );
        store.store("oauth-provider", Credential::Token { token: "t".into() });
        store.activate("anthropic");

        let json = store.to_json().expect("encode");
        let restored = AuthStore::from_json(&json).expect("decode");
        assert_eq!(restored, store);
        assert_eq!(
            restored.connected(),
            vec!["anthropic", "custom", "oauth-provider"]
        );
    }

    #[test]
    fn an_empty_store_round_trips_and_rejects_garbage() {
        let store = AuthStore::new();
        let json = store.to_json().expect("encode");
        assert_eq!(AuthStore::from_json(&json).expect("decode"), store);
        assert!(AuthStore::from_json("not json").is_err());
    }

    #[test]
    fn credential_kinds_and_masked_forms() {
        assert_eq!(Credential::ApiKey { key: "k".into() }.kind(), "api key");
        assert_eq!(
            Credential::Endpoint {
                base_url: "https://x/v1".into(),
                api_key: "k".into()
            }
            .kind(),
            "endpoint"
        );
        assert_eq!(
            Credential::Token { token: "t".into() }.kind(),
            "oauth token"
        );

        // The endpoint is not a secret, so it is shown as-is.
        assert_eq!(
            Credential::Endpoint {
                base_url: "https://x/v1".into(),
                api_key: "secret".into()
            }
            .masked(),
            "https://x/v1"
        );
        assert_eq!(
            Credential::ApiKey {
                key: "sk-12345".into()
            }
            .masked(),
            "\u{2022}\u{2022}\u{2022}\u{2022}2345"
        );
    }
}
