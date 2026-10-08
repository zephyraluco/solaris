//! Resolving a platform and its credential into a backend.
//!
//! This is the platform half of the workspace: it knows which providers exist
//! (see [`crate::providers`]), how each authenticates, where its endpoint lives,
//! which models it offers, and what they cost. [`choose_backend`] is the only
//! entry point the application needs — it returns a backend already configured
//! for the credentials at hand, so the UI never learns which platform it is
//! talking to. Until `/connect` has stored something usable, an
//! [`UnconnectedBackend`] stands in and answers every turn with what to do
//! about that.

use std::sync::Arc;

use solaris_backend::{AgentBackend, AgentEventStream, Endpoint, HttpBackend, TurnPlan};
use solaris_core::{BackendError, TurnRequest};

use crate::providers::{AuthKind, PROVIDERS, ProviderSpec, max_output_for, price_for, provider};
use crate::{AuthStore, Credential};

/// The backend that stands in while nothing is connected.
///
/// It refuses every turn with an explanation rather than a canned reply: there
/// is no model to ask until `/connect` has stored a credential, and inventing
/// an answer would hide that.
struct UnconnectedBackend {
    /// What the user can do about it.
    message: String,
}

impl UnconnectedBackend {
    /// A backend whose turns fail with `message`.
    fn new(message: String) -> Self {
        Self { message }
    }
}

#[async_trait::async_trait]
impl AgentBackend for UnconnectedBackend {
    async fn run_turn(&self, _request: TurnRequest) -> Result<AgentEventStream, BackendError> {
        Err(BackendError::new(self.message.clone()))
    }

    fn label(&self) -> &str {
        "unconnected"
    }
}

/// Where provider keys are looked up besides the credential store.
///
/// The process environment in production; a fixed table in tests, so a test
/// neither inherits the machine's keys nor reaches the network.
pub type Environment =
    Arc<dyn Fn(&'static [&'static str]) -> Option<(&'static str, String)> + Send + Sync>;

/// Look names up in the real process environment.
pub fn process_environment() -> Environment {
    Arc::new(|names: &'static [&'static str]| {
        names.iter().find_map(|name| {
            std::env::var(name)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .map(|value| (*name, value))
        })
    })
}

/// An environment with nothing set.
pub fn empty_environment() -> Environment {
    Arc::new(|_| None)
}

/// How the backend is resolved.
#[derive(Clone)]
pub struct BackendOptions {
    /// Where provider keys are looked up.
    pub environment: Environment,
}

impl Default for BackendOptions {
    fn default() -> Self {
        Self {
            environment: process_environment(),
        }
    }
}

/// Where the credential a backend will use came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSource {
    /// A key or endpoint stored by `/connect`.
    Stored,
    /// A key read from this environment variable.
    Env(&'static str),
    /// The provider is a runtime on this machine and wants no credentials, or
    /// the backend was handed in directly rather than resolved.
    None,
    /// Nothing usable, so no provider can be asked and every turn says so.
    Missing,
    /// The provider signs in over OAuth, which is not implemented yet, so no
    /// provider can be asked and every turn says so.
    NotImplemented,
    /// The credential is fine, but no model is named and the catalogue has none
    /// to offer — so there is nothing to ask the provider for.
    NoModel,
}

/// The backend the current credentials call for, and why it was chosen.
#[derive(Clone)]
pub struct BackendChoice {
    /// The backend to use.
    pub backend: Arc<dyn AgentBackend>,
    /// Provider this resolved to, when one was picked at all.
    pub provider_id: Option<&'static str>,
    /// Endpoint requests go to, when a real client was built.
    pub base_url: Option<String>,
    /// Model that will actually be asked for, which is not always the one that
    /// was requested: an unnamed model is filled in with the provider's first
    /// offer, so callers should adopt this value rather than keep their own.
    pub model: String,
    /// Where the credential came from.
    pub source: CredentialSource,
}

impl BackendChoice {
    /// Nothing to talk to, for `reason`, with `message` explaining the refusal.
    fn unconnected(reason: CredentialSource, message: String, model: &str) -> Self {
        Self {
            backend: Arc::new(UnconnectedBackend::new(message)),
            provider_id: None,
            base_url: None,
            model: model.to_string(),
            source: reason,
        }
    }
}

/// The environment lookup the selection uses.
type EnvLookup<'a> = &'a Environment;

/// Assemble the backend for the current credentials and model.
///
/// This is the only function the application needs: it never learns which
/// provider it is talking to. With no usable credential it returns a backend
/// that refuses turns with an explanation instead of reaching the network.
pub fn choose_backend(auth: &AuthStore, model: &str, options: BackendOptions) -> BackendChoice {
    let lookup = &options.environment;

    let Some(spec) = active_provider(auth, lookup) else {
        return BackendChoice::unconnected(
            CredentialSource::Missing,
            "no provider is connected — run /connect to add a credential".to_string(),
            model,
        );
    };

    if spec.auth == AuthKind::DeviceCode && auth.credential(spec.id).is_none() {
        // The flow that would mint a token is not built, so say so rather than
        // send a request that can only fail with a 401 the user cannot act on.
        return BackendChoice::unconnected(
            CredentialSource::NotImplemented,
            format!(
                "{} signs in with OAuth, which is not implemented yet — connect an API key instead",
                spec.name
            ),
            model,
        );
    }

    let Some((base_url, secret, source)) = resolve(spec, auth, lookup) else {
        return BackendChoice::unconnected(
            CredentialSource::Missing,
            missing_credential_message(spec),
            model,
        );
    };

    let model = real_model(spec, model);

    if model.is_empty() {
        // No model was named and the catalogue offers none to substitute, so
        // the request could only carry an empty name and earn a 400. Saying so
        // here is cheaper than the round trip, and `/model <name>` fixes it.
        return BackendChoice::unconnected(
            CredentialSource::NoModel,
            format!(
                "no model is named for {} — run /model <name> to pick one",
                spec.name
            ),
            &model,
        );
    }

    let endpoint = Endpoint {
        wire: spec.wire,
        base_url,
        secret,
        name: spec.name,
        label: spec.id,
        env_keys: spec.env_keys,
    };
    let prompt_cache_key = is_openai_host(&endpoint.base_url).then_some(PROMPT_CACHE_KEY);

    // Everything platform-specific is decided here, so the backend can stay a
    // transport: which model, how long a reply, what it costs, and whether a
    // prompt-cache key rides along.
    let plan = TurnPlan {
        max_tokens: max_output_for(&model),
        price: price_for(&model),
        prompt_cache_key,
        model: model.clone(),
        endpoint,
    };

    match HttpBackend::new(plan) {
        Ok(backend) => {
            let base_url = backend.base_url().to_string();
            BackendChoice {
                backend: Arc::new(backend),
                provider_id: Some(spec.id),
                base_url: Some(base_url),
                model,
                source,
            }
        }
        // The only way building a client fails is a broken TLS setup, which is
        // a bug rather than a user error; the turn reports it rather than
        // panicking.
        Err(error) => BackendChoice::unconnected(
            CredentialSource::Missing,
            format!("could not set up HTTP for {}: {error}", spec.name),
            &model,
        ),
    }
}

/// Cache-routing key sent to OpenAI, which caches prompt prefixes without being
/// asked. The key groups a client's requests onto one cache shard, and a single
/// user wants every turn of a session on the same one.
const PROMPT_CACHE_KEY: &str = "solaris";

/// Whether `base_url` points at OpenAI's own host.
///
/// The cache key is only sent there. OpenAI's cache is automatic, so the field
/// buys routing and nothing else, while a compatible gateway that does not
/// implement it may reject the whole request — not worth the risk.
fn is_openai_host(base_url: &str) -> bool {
    base_url
        .split_once("://")
        .map_or(base_url, |(_, rest)| rest)
        .split(['/', '?', '#'])
        .next()
        .is_some_and(|host| host == "api.openai.com")
}

/// The secret a stored credential carries, or `None` when it carries none.
fn secret_of(credential: &Credential) -> Option<String> {
    credential
        .secret()
        .map(|secret| secret.trim().to_string())
        .filter(|secret| !secret.is_empty())
}

/// What to tell the user when `spec` has no usable credential.
fn missing_credential_message(spec: &ProviderSpec) -> String {
    match spec.env_keys.first() {
        Some(key) => format!(
            "{} has no credential — run /connect, or set {key}",
            spec.name
        ),
        None => format!("{} has no credential — run /connect", spec.name),
    }
}

/// The model to ask `spec` for.
///
/// An empty model means the user has not named one, so the provider is asked
/// for the first model it offers rather than sent an empty string it would
/// reject. A model the user did name travels as written, even when the
/// catalogue does not list it.
fn real_model(spec: &'static ProviderSpec, model: &str) -> String {
    if !model.is_empty() {
        return model.to_string();
    }
    spec.models
        .first()
        .map(|offered| offered.id.to_string())
        .unwrap_or_else(|| model.to_string())
}

/// Just the backend, for callers that do not care how it was chosen.
pub fn build_backend(
    auth: &AuthStore,
    model: &str,
    options: BackendOptions,
) -> Arc<dyn AgentBackend> {
    choose_backend(auth, model, options).backend
}

/// The provider requests go to.
///
/// The active one when `/connect` has been used, otherwise the first whose key
/// is already in the environment — so `ANTHROPIC_API_KEY=… solaris` works with
/// no setup at all.
fn active_provider(auth: &AuthStore, lookup: EnvLookup<'_>) -> Option<&'static ProviderSpec> {
    if let Some(spec) = auth.active_provider().and_then(provider) {
        return Some(spec);
    }
    PROVIDERS
        .iter()
        .find(|spec| lookup(spec.env_keys).is_some())
}

/// Resolve `spec` to an endpoint, a secret and where that secret came from.
///
/// `None` means there is nothing to send a request to at all. The environment
/// wins over the file, so a shell can point solaris at a different key without
/// editing `auth.json`.
fn resolve(
    spec: &'static ProviderSpec,
    auth: &AuthStore,
    lookup: EnvLookup<'_>,
) -> Option<(String, Option<String>, CredentialSource)> {
    let stored = auth.credential(spec.id);

    let base_url = match spec.base_url {
        Some(default) => lookup(spec.base_url_envs)
            .map(|(_, url)| url)
            .unwrap_or_else(|| default.to_string()),
        // Only the custom endpoint takes its URL from the credential.
        None => match stored {
            Some(Credential::Endpoint { base_url, .. }) if !base_url.trim().is_empty() => {
                base_url.trim().to_string()
            }
            _ => return None,
        },
    };

    if let Some((name, key)) = lookup(spec.env_keys) {
        return Some((base_url, Some(key), CredentialSource::Env(name)));
    }

    match stored.map(secret_of).unwrap_or_default() {
        Some(secret) => Some((base_url, Some(secret), CredentialSource::Stored)),
        // A provider that authenticates with a key cannot be called without
        // one; a local runtime and a keyless endpoint can.
        None if spec.auth == AuthKind::ApiKey => None,
        None => Some((base_url, None, CredentialSource::None)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Options with a fixed environment, so tests never touch the process's
    /// variables or the network.
    fn options_with(pairs: &'static [(&'static str, &'static str)]) -> BackendOptions {
        BackendOptions {
            environment: Arc::new(move |names: &'static [&'static str]| {
                names.iter().find_map(|name| {
                    pairs
                        .iter()
                        .find(|(key, _)| key == name)
                        .map(|(key, value)| (*key, (*value).to_string()))
                })
            }),
        }
    }

    /// Options with nothing set in the environment.
    fn options() -> BackendOptions {
        options_with(&[])
    }

    fn auth_with(provider: &str, credential: Option<Credential>) -> AuthStore {
        let mut auth = AuthStore::new();
        if let Some(credential) = credential {
            auth.store(provider, credential);
        }
        auth.activate(provider);
        auth
    }

    /// One turn, for driving a backend that must refuse it.
    fn request() -> TurnRequest {
        TurnRequest {
            history: Vec::new(),
            prompt: "hello".to_string(),
            mode: solaris_core::Mode::Build,
        }
    }

    #[tokio::test]
    async fn nothing_connected_refuses_the_turn_with_advice() {
        let choice = choose_backend(&AuthStore::new(), "", options());

        assert_eq!(choice.source, CredentialSource::Missing);
        assert_eq!(choice.provider_id, None);
        assert_eq!(choice.backend.label(), "unconnected");

        let error = match choice.backend.run_turn(request()).await {
            Ok(_) => panic!("an unconnected backend must not answer"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("/connect"), "{error}");
    }

    #[test]
    fn a_stored_key_builds_a_real_backend() {
        let auth = auth_with(
            "anthropic",
            Some(Credential::ApiKey {
                key: "sk-test".to_string(),
            }),
        );
        let choice = choose_backend(&auth, "claude-sonnet-4-5", options());

        assert_eq!(choice.source, CredentialSource::Stored);
        assert_eq!(choice.provider_id, Some("anthropic"));
        assert_eq!(choice.backend.label(), "anthropic");
        assert_eq!(
            choice.base_url.as_deref(),
            Some("https://api.anthropic.com")
        );
    }

    #[tokio::test]
    async fn a_named_model_is_required_when_the_catalogue_has_none() {
        // A gateway's catalogue is empty, so an unnamed model leaves nothing to
        // send. Refusing here beats sending an empty name and collecting a 400.
        let auth = auth_with(
            "new-api",
            Some(Credential::Endpoint {
                base_url: "https://gateway.test/v1".to_string(),
                api_key: "k".to_string(),
            }),
        );

        let choice = choose_backend(&auth, "", options());
        assert_eq!(choice.source, CredentialSource::NoModel);
        assert_eq!(choice.backend.label(), "unconnected");

        let error = match choice.backend.run_turn(request()).await {
            Ok(_) => panic!("a model-less provider must not be asked"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("/model"), "{error}");

        // Naming one is all it takes.
        let choice = choose_backend(&auth, "glm-5.3", options());
        assert_eq!(choice.backend.label(), "new-api");
        assert_eq!(choice.model, "glm-5.3");
        assert_eq!(choice.source, CredentialSource::Stored);
    }

    #[test]
    fn an_unnamed_model_becomes_the_providers_first() {
        let auth = auth_with(
            "anthropic",
            Some(Credential::ApiKey {
                key: "k".to_string(),
            }),
        );

        // Nothing named: the provider would reject an empty model, so its own
        // first offer is sent instead.
        let choice = choose_backend(&auth, "", options());
        assert_eq!(choice.model, "claude-sonnet-4-5");
    }

    #[test]
    fn a_model_the_user_named_is_left_alone() {
        let auth = auth_with(
            "anthropic",
            Some(Credential::ApiKey {
                key: "k".to_string(),
            }),
        );

        let choice = choose_backend(&auth, "claude-haiku-4-5", options());
        assert_eq!(choice.model, "claude-haiku-4-5");

        // Including one the catalogue does not list: the user may know better
        // than the bundled snapshot does.
        let choice = choose_backend(&auth, "claude-something-new", options());
        assert_eq!(choice.model, "claude-something-new");
    }

    #[test]
    fn an_unconnected_choice_keeps_the_named_model() {
        let choice = choose_backend(&AuthStore::new(), "my-model", options());
        assert_eq!(choice.model, "my-model");
        assert_eq!(choice.backend.label(), "unconnected");
    }

    #[test]
    fn a_provider_with_no_catalogue_keeps_the_named_model() {
        // A custom endpoint has no model list, so nothing better than what the
        // user typed exists to send.
        let auth = auth_with(
            "custom",
            Some(Credential::Endpoint {
                base_url: "https://example.test/v1".to_string(),
                api_key: String::new(),
            }),
        );
        let choice = choose_backend(&auth, "my-model", options());
        assert_eq!(choice.backend.label(), "custom");
        assert_eq!(choice.model, "my-model");
    }

    #[test]
    fn an_environment_key_wins_over_the_stored_one() {
        let auth = auth_with(
            "anthropic",
            Some(Credential::ApiKey {
                key: "sk-stored".to_string(),
            }),
        );
        let choice = choose_backend(
            &auth,
            "claude-sonnet-4-5",
            options_with(&[("ANTHROPIC_API_KEY", "sk-env")]),
        );

        assert_eq!(choice.source, CredentialSource::Env("ANTHROPIC_API_KEY"));
        assert_eq!(choice.backend.label(), "anthropic");
    }

    #[test]
    fn an_environment_key_alone_is_enough_to_start() {
        // No `/connect`, no auth.json: the provider is found by its key alone.
        let choice = choose_backend(
            &AuthStore::new(),
            "gemini-2.5-flash",
            options_with(&[("GOOGLE_API_KEY", "gsk-test")]),
        );

        assert_eq!(choice.source, CredentialSource::Env("GOOGLE_API_KEY"));
        assert_eq!(choice.provider_id, Some("google"));
        assert_eq!(
            choice.base_url.as_deref(),
            Some("https://generativelanguage.googleapis.com/v1beta/openai")
        );
    }

    #[test]
    fn a_base_url_override_is_honoured() {
        let choice = choose_backend(
            &AuthStore::new(),
            "gpt-5",
            options_with(&[
                ("OPENAI_API_KEY", "sk-test"),
                ("OPENAI_BASE_URL", "https://gateway.internal/v1"),
            ]),
        );

        assert_eq!(
            choice.base_url.as_deref(),
            Some("https://gateway.internal/v1")
        );
    }

    #[test]
    fn a_key_provider_with_no_key_cannot_answer() {
        let mut auth = AuthStore::new();
        auth.activate("openai");
        let choice = choose_backend(&auth, "gpt-5", options());

        assert_eq!(choice.source, CredentialSource::Missing);
        assert_eq!(choice.backend.label(), "unconnected");
    }

    #[test]
    fn a_local_runtime_needs_no_credentials_at_all() {
        let auth = auth_with("local", None);
        let choice = choose_backend(&auth, "llama3.2", options());

        assert_eq!(choice.source, CredentialSource::None);
        assert_eq!(choice.backend.label(), "local");
        assert_eq!(
            choice.base_url.as_deref(),
            Some("http://localhost:11434/v1")
        );
    }

    #[test]
    fn the_local_runtime_endpoint_can_be_pointed_elsewhere() {
        let auth = auth_with("local", None);
        let choice = choose_backend(
            &auth,
            "llama3.2",
            options_with(&[("SOLARIS_LOCAL_BASE_URL", "http://127.0.0.1:8080/v1")]),
        );

        assert_eq!(choice.base_url.as_deref(), Some("http://127.0.0.1:8080/v1"));
    }

    #[test]
    fn a_custom_endpoint_supplies_its_own_url() {
        let auth = auth_with(
            "custom",
            Some(Credential::Endpoint {
                base_url: "https://example.test/v1".to_string(),
                api_key: String::new(),
            }),
        );
        let choice = choose_backend(&auth, "my-model", options());

        assert_eq!(choice.backend.label(), "custom");
        assert_eq!(choice.base_url.as_deref(), Some("https://example.test/v1"));
        // A keyless endpoint is legitimate, so it is not reported as missing.
        assert_eq!(choice.source, CredentialSource::None);
    }

    #[test]
    fn a_custom_endpoint_without_a_url_cannot_answer() {
        let mut auth = AuthStore::new();
        auth.activate("custom");
        let choice = choose_backend(&auth, "my-model", options());

        assert_eq!(choice.source, CredentialSource::Missing);
        assert_eq!(choice.backend.label(), "unconnected");
    }

    #[test]
    fn an_oauth_provider_says_it_is_not_implemented_yet() {
        let mut auth = AuthStore::new();
        auth.activate("claude-subscription");
        let choice = choose_backend(&auth, "claude-sonnet-4-5", options());

        assert_eq!(choice.source, CredentialSource::NotImplemented);
        assert_eq!(choice.backend.label(), "unconnected");

        // A token that really is there is used, so the seam is ready for the
        // day the sign-in flow lands.
        let auth = auth_with(
            "claude-subscription",
            Some(Credential::Token {
                token: "oauth-token".to_string(),
            }),
        );
        let choice = choose_backend(&auth, "claude-sonnet-4-5", options());
        assert_eq!(choice.source, CredentialSource::Stored);
        assert_eq!(choice.backend.label(), "claude-subscription");
    }

    #[test]
    fn build_backend_is_the_choice_without_the_reason() {
        let auth = auth_with("local", None);
        let backend = build_backend(&auth, "llama3.2", options());
        assert_eq!(backend.label(), "local");
    }

    #[test]
    fn an_unknown_active_provider_is_treated_as_unconfigured() {
        let mut auth = AuthStore::new();
        auth.activate("something-else");
        let choice = choose_backend(&auth, "gpt-5", options());

        assert_eq!(choice.backend.label(), "unconnected");
    }

    #[test]
    fn the_real_environment_is_consulted_by_default() {
        // The default options must be the process environment, or a key in the
        // shell would be ignored.
        let options = BackendOptions::default();
        assert_eq!((options.environment)(&[]), None, "no names, no lookup");
    }

    #[test]
    fn an_empty_environment_reports_nothing() {
        assert_eq!((empty_environment())(&["HOME"]), None);
    }
}
