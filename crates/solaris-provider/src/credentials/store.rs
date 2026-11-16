//! Application credential persistence; protocol authentication belongs to the SDK.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A stored credential.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Credential {
    /// A provider API key.
    ApiKey { key: String },
    /// An OpenAI-compatible endpoint and its optional key.
    Endpoint { base_url: String, api_key: String },
    /// An OAuth access token.
    Token { token: String },
    /// ChatGPT integration credentials, not an OpenAI API key.
    ChatGpt {
        access_token: String,
        refresh_token: String,
        id_token: String,
        expires_at: u64,
        account_id: String,
    },
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("kind", &self.kind())
            .finish_non_exhaustive()
    }
}

impl Credential {
    /// Short label for the kind of credential.
    pub fn kind(&self) -> &'static str {
        match self {
            Credential::ApiKey { .. } => "api key",
            Credential::Endpoint { .. } => "endpoint",
            Credential::Token { .. } => "oauth token",
            Credential::ChatGpt { .. } => "ChatGPT OAuth",
        }
    }

    /// Redacted form, safe to print.
    pub fn masked(&self) -> String {
        match self {
            Credential::ApiKey { key } => mask_secret(key),
            Credential::Endpoint { base_url, .. } => base_url.clone(),
            Credential::Token { token } => mask_secret(token),
            Credential::ChatGpt { .. } => "ChatGPT (authorized)".to_string(),
        }
    }

    /// The secret itself, when the credential carries one.
    pub fn secret(&self) -> Option<&str> {
        match self {
            Credential::ApiKey { key } => Some(key),
            Credential::Endpoint { api_key, .. } => Some(api_key),
            Credential::Token { token } => Some(token),
            Credential::ChatGpt { access_token, .. } => Some(access_token),
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

/// Credentials for this installation plus the active provider and model.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthStore {
    #[serde(default)]
    entries: BTreeMap<String, Credential>,
    #[serde(default)]
    active: Option<Active>,
}

static AUTH_WRITES: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

    /// Atomically replace private credentials, including rotating OAuth tokens.
    pub fn save(&self, path: &std::path::Path) -> Result<(), AuthError> {
        let _guard = AUTH_WRITES
            .lock()
            .map_err(|_| std::io::Error::other("credential write lock poisoned"))?;
        self.save_unlocked(path)
    }
    /// Apply explicit changes to the latest snapshot, preserving concurrently rotated tokens.
    pub fn save_changes(&self, base: &Self, path: &std::path::Path) -> Result<Self, AuthError> {
        self.merge_file(base, path, |_, _| true)
    }
    /// A background rotation must not resurrect a credential removed or replaced by the user.
    pub(crate) fn save_refreshed(
        &self,
        base: &Self,
        path: &std::path::Path,
    ) -> Result<Self, AuthError> {
        self.merge_file(base, path, |current, previous| current == previous)
    }
    fn merge_file(
        &self,
        base: &Self,
        path: &std::path::Path,
        replace: impl Fn(Option<&Credential>, Option<&Credential>) -> bool,
    ) -> Result<Self, AuthError> {
        let _guard = AUTH_WRITES
            .lock()
            .map_err(|_| std::io::Error::other("credential write lock poisoned"))?;
        let mut latest = match std::fs::read_to_string(path) {
            Ok(text) => Self::from_json(&text)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => base.clone(),
            Err(error) => return Err(error.into()),
        };
        let keys: std::collections::BTreeSet<_> =
            base.entries.keys().chain(self.entries.keys()).collect();
        for key in keys {
            if self.entries.get(key) != base.entries.get(key)
                && replace(latest.entries.get(key), base.entries.get(key))
            {
                if let Some(value) = self.entries.get(key) {
                    latest.entries.insert(key.clone(), value.clone());
                } else {
                    latest.entries.remove(key);
                }
            }
        }
        if self.active != base.active {
            latest.active = self.active.clone();
        }
        latest.save_unlocked(path)?;
        Ok(latest)
    }
    fn save_unlocked(&self, path: &std::path::Path) -> Result<(), AuthError> {
        use std::io::Write;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new("."));
        std::fs::create_dir_all(parent)?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.write_all(self.to_json()?.as_bytes())?;
        file.as_file().sync_all()?;
        file.persist(path).map_err(|error| error.error)?;
        #[cfg(unix)]
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    }

    /// Parse a previously serialized store.
    pub fn from_json(text: &str) -> Result<Self, AuthError> {
        serde_json::from_str(text).map_err(|error| AuthError::Decode(error.to_string()))
    }
}

/// Failures raised while encoding, decoding or saving the store.
#[derive(Debug, Error)]
pub enum AuthError {
    #[error("could not save credentials: {0}")]
    Write(#[from] std::io::Error),
    #[error("could not encode credentials: {0}")]
    Encode(String),
    #[error("could not read credentials: {0}")]
    Decode(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_edits_preserve_rotated_credentials_and_refresh_preserves_ui_selection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let mut base = AuthStore::new();
        base.store(
            "openai",
            Credential::Token {
                token: "old".into(),
            },
        );
        base.activate("openai");
        base.save(&path).unwrap();
        let mut refreshed = base.clone();
        refreshed.store(
            "openai",
            Credential::Token {
                token: "rotated".into(),
            },
        );
        refreshed.save_changes(&base, &path).unwrap();
        let mut ui = base.clone();
        ui.store(
            "anthropic",
            Credential::ApiKey {
                key: "new-provider".into(),
            },
        );
        ui.activate("anthropic");
        let merged = ui.save_changes(&base, &path).unwrap();
        assert_eq!(merged.credential("openai"), refreshed.credential("openai"));
        assert_eq!(merged.active_provider(), Some("anthropic"));
        let mut next = refreshed.clone();
        next.store(
            "openai",
            Credential::Token {
                token: "rotated-again".into(),
            },
        );
        let merged = next.save_changes(&refreshed, &path).unwrap();
        assert_eq!(merged.active_provider(), Some("anthropic"));
        assert!(merged.credential("anthropic").is_some());
    }

    #[test]
    fn chatgpt_credentials_round_trip_without_debug_leaks() {
        let credential = Credential::ChatGpt {
            access_token: "private-access".into(),
            refresh_token: "private-refresh".into(),
            id_token: "private-id".into(),
            expires_at: 1234,
            account_id: "account".into(),
        };
        let mut auth = AuthStore::new();
        auth.store("openai", credential);
        auth.activate("openai");
        assert_eq!(
            AuthStore::from_json(&auth.to_json().unwrap()).unwrap(),
            auth
        );
        let debug = format!("{auth:?}");
        assert!(!debug.contains("private-"));
        assert_eq!(
            auth.active_provider()
                .and_then(|id| auth.credential(id))
                .unwrap()
                .masked(),
            "ChatGPT (authorized)"
        );
    }

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
        assert!(store.credential("anthropic").is_some());
        assert!(store.credential("openai").is_none());
        assert_eq!(store.len(), 1);

        store.activate("anthropic");
        assert_eq!(store.active_provider(), Some("anthropic"));
        assert_eq!(
            store
                .active_provider()
                .and_then(|id| store.credential(id))
                .and_then(Credential::secret),
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
    fn bare_provider_selection_is_not_an_auth_store_format() {
        assert!(AuthStore::from_json(r#"{"entries":{},"active":"anthropic"}"#).is_err());
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
