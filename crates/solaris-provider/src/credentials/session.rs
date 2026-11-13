//! Shared OAuth refresh for noninteractive sessions and their child backends.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use solaris_backend::{BackendError, ModelCapabilities, ModelConnector};
use tokio::sync::Mutex;

use crate::{AuthStore, BackendChoice, BackendOptions, Credential, choose_backend, oauth};

type Factory = Arc<dyn Fn(&AuthStore, &str) -> BackendChoice + Send + Sync>;
type Refresh = Arc<
    dyn Fn(Credential) -> Pin<Box<dyn Future<Output = Result<Credential, String>> + Send>>
        + Send
        + Sync,
>;

/// Resolve backends that refresh saved ChatGPT credentials before each request.
/// All models share refresh state; no model request is retried after execution.
pub fn refreshing_backend_factory(
    auth: AuthStore,
    auth_path: Option<PathBuf>,
    options: BackendOptions,
) -> impl Fn(&str) -> BackendChoice + Send + Sync {
    let factory: Factory =
        Arc::new(move |auth, model| choose_backend(auth, model, options.clone()));
    let refresh: Refresh = Arc::new(|credential| {
        Box::pin(async move {
            oauth::refresh(&credential)
                .await
                .map_err(|error| error.to_string())
        })
    });
    let shared = Arc::new(Credentials::new(auth, auth_path, factory, refresh));
    move |model| shared.choice(model)
}

struct Credentials {
    initial: AuthStore,
    path: Option<PathBuf>,
    factory: Factory,
    refresh: Refresh,
    current: Mutex<State>,
}

struct State {
    auth: AuthStore,
    dirty: bool,
    backends: HashMap<String, BackendChoice>,
}

impl Credentials {
    fn new(auth: AuthStore, path: Option<PathBuf>, factory: Factory, refresh: Refresh) -> Self {
        Self {
            initial: auth.clone(),
            path,
            factory,
            refresh,
            current: Mutex::new(State {
                auth,
                dirty: false,
                backends: HashMap::new(),
            }),
        }
    }

    fn choice(self: &Arc<Self>, model: &str) -> BackendChoice {
        let mut choice = (self.factory)(&self.initial, model);
        if choice.provider_id.as_deref() == Some("openai")
            && matches!(
                self.initial.credential("openai"),
                Some(Credential::ChatGpt { .. })
            )
        {
            choice.models = Arc::new(RefreshingModels {
                model: choice.model.clone(),
                credentials: Arc::clone(self),
                initial: Arc::clone(&choice.models),
            });
            choice.backend = Arc::new(RefreshingBackend {
                model: choice.model.clone(),
                initial: choice.backend,
                credentials: Arc::clone(self),
            });
        }
        choice
    }

    async fn backend(&self, model: &str) -> Result<BackendChoice, BackendError> {
        // Serialize refresh only; release this guard before model/network work.
        let mut state = self.current.lock().await;
        if let Some(credential) = state
            .auth
            .credential("openai")
            .filter(|c| oauth::needs_refresh(c))
            .cloned()
        {
            if self.path.is_none() {
                return Err(BackendError::new(
                    "cannot refresh OpenAI credentials without an auth file",
                ));
            }
            let credential = (self.refresh)(credential)
                .await
                .map_err(BackendError::new)?;
            state.auth.store("openai", credential);
            state.dirty = true;
            state.backends.clear();
        }
        if state.dirty {
            let path = self
                .path
                .as_deref()
                .ok_or_else(|| BackendError::new("cannot save refreshed OpenAI credentials"))?;
            // Keep rotated credentials in memory on failure so a later request
            // retries saving, not an exchange using the obsolete refresh token.
            state
                .auth
                .save(path)
                .map_err(|error| BackendError::new(error.to_string()))?;
            state.dirty = false;
        }
        if !state.backends.contains_key(model) {
            let choice = (self.factory)(&state.auth, model);
            state.backends.insert(model.to_owned(), choice);
        }
        Ok(state.backends[model].clone())
    }
}

struct RefreshingBackend {
    model: String,
    initial: Arc<dyn ModelConnector>,
    credentials: Arc<Credentials>,
}

#[async_trait]
impl ModelConnector for RefreshingBackend {
    async fn model_client(self: Arc<Self>) -> Result<solaris_backend::ModelClient, BackendError> {
        self.credentials
            .backend(&self.model)
            .await?
            .backend
            .model_client()
            .await
    }

    fn label(&self) -> &str {
        self.initial.label()
    }
    fn capabilities(&self) -> ModelCapabilities {
        self.initial.capabilities()
    }
}

struct RefreshingModels {
    model: String,
    initial: Arc<dyn crate::ModelSource>,
    credentials: Arc<Credentials>,
}
#[async_trait]
impl crate::ModelSource for RefreshingModels {
    async fn models(&self) -> Result<Vec<String>, BackendError> {
        self.credentials
            .backend(&self.model)
            .await?
            .models
            .models()
            .await
    }
    fn notice(&self) -> Option<String> {
        self.credentials
            .current
            .try_lock()
            .ok()
            .and_then(|state| {
                state
                    .backends
                    .get(&self.model)
                    .and_then(|choice| choice.models.notice())
            })
            .or_else(|| self.initial.notice())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CredentialSource;
    use solaris_agent::AgentEventStream;
    use solaris_agent::testing::TurnRequest;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn credential(token: &str, expires_at: u64) -> Credential {
        Credential::ChatGpt {
            access_token: token.into(),
            refresh_token: format!("refresh-{token}"),
            id_token: "id".into(),
            account_id: "account".into(),
            expires_at,
        }
    }
    struct Recording {
        token: String,
        calls: Arc<StdMutex<Vec<String>>>,
    }
    #[async_trait]
    impl ModelConnector for Recording {
        async fn model_client(
            self: std::sync::Arc<Self>,
        ) -> Result<solaris_backend::ModelClient, solaris_backend::BackendError> {
            Ok(solaris_agent::testing::client(
                self.capabilities(),
                move |request| {
                    let fixture = std::sync::Arc::clone(&self);
                    async move { fixture.respond(request).await }
                },
            ))
        }
    }
    impl Recording {
        async fn respond(&self, _: TurnRequest) -> Result<AgentEventStream, BackendError> {
            self.calls.lock().expect("calls").push(self.token.clone());
            Ok(Box::pin(futures::stream::iter([
                solaris_agent::AgentEvent::TurnComplete {
                    usage: Default::default(),
                    cost_usd: 0.0,
                },
            ])))
        }
    }

    fn setup(path: PathBuf, refresh: Refresh) -> (Arc<Credentials>, Arc<StdMutex<Vec<String>>>) {
        let mut auth = AuthStore::new();
        auth.activate("openai");
        auth.store("openai", credential("expired", 0));
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let recorded = Arc::clone(&calls);
        let factory: Factory = Arc::new(move |auth, model| {
            let Credential::ChatGpt { access_token, .. } =
                auth.credential("openai").expect("credential")
            else {
                panic!("OAuth credential")
            };
            BackendChoice {
                models: Arc::new(crate::UnavailableModels),
                configuration: Default::default(),
                backend: Arc::new(Recording {
                    token: access_token.clone(),
                    calls: Arc::clone(&recorded),
                }),
                provider_id: Some("openai".into()),
                base_url: None,
                model: model.into(),
                source: CredentialSource::Stored,
            }
        });
        (
            Arc::new(Credentials::new(auth, Some(path), factory, refresh)),
            calls,
        )
    }
    fn request() -> TurnRequest {
        TurnRequest::new(Vec::new(), "prompt")
    }

    #[tokio::test]
    async fn preparing_parent_and_child_connections_shares_one_refresh_without_model_io() {
        let directory = tempfile::tempdir().unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&count);
        let (shared, calls) = setup(
            directory.path().join("auth.json"),
            Arc::new(move |_| {
                observed.fetch_add(1, Ordering::SeqCst);
                Box::pin(async {
                    tokio::task::yield_now().await;
                    Ok(credential("fresh", u64::MAX))
                })
            }),
        );
        let parent = shared.choice("parent");
        let child = shared.choice("child");
        let (one, two) = tokio::join!(parent.backend.model_client(), child.backend.model_client());
        assert!(one.is_ok() && two.is_ok());
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert!(calls.lock().unwrap().is_empty());
        let saved = AuthStore::from_json(
            &std::fs::read_to_string(directory.path().join("auth.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            saved.credential("openai"),
            Some(&credential("fresh", u64::MAX))
        );
    }

    #[tokio::test]
    async fn expired_credentials_are_refreshed_and_saved_for_parent_and_children() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("auth.json");
        let count = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&count);
        let refresh: Refresh = Arc::new(move |old| {
            assert!(oauth::needs_refresh(&old));
            observed.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                tokio::task::yield_now().await;
                Ok(credential("fresh", u64::MAX))
            })
        });
        let (shared, calls) = setup(path.clone(), refresh);
        shared.initial.save(&path).expect("original credentials");
        let parent = shared.choice("parent");
        let child = shared.choice("child");
        let (one, two) = tokio::join!(
            solaris_agent::testing::request(parent.backend, request()),
            solaris_agent::testing::request(child.backend, request())
        );
        drop(one.expect("parent request"));
        drop(two.expect("child request"));
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(*calls.lock().expect("calls"), vec!["fresh", "fresh"]);
        let persisted =
            AuthStore::from_json(&std::fs::read_to_string(&path).expect("saved credentials"))
                .expect("auth");
        assert_eq!(
            persisted.credential("openai"),
            Some(&credential("fresh", u64::MAX))
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path)
                    .expect("metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[tokio::test]
    async fn a_long_lived_backend_refreshes_again_when_credentials_expire_between_prompts() {
        let directory = tempfile::tempdir().expect("directory");
        let count = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&count);
        let refresh: Refresh = Arc::new(move |_| {
            let number = observed.fetch_add(1, Ordering::SeqCst) + 1;
            Box::pin(async move { Ok(credential(&format!("fresh-{number}"), u64::MAX)) })
        });
        let (shared, calls) = setup(directory.path().join("auth.json"), refresh);
        let backend = shared.choice("model").backend;
        drop(
            solaris_agent::testing::request(Arc::clone(&backend), request())
                .await
                .expect("first prompt"),
        );
        let mut state = shared.current.lock().await;
        let mut expired = state.auth.credential("openai").expect("credential").clone();
        if let Credential::ChatGpt { expires_at, .. } = &mut expired {
            *expires_at = 0;
        }
        state.auth.store("openai", expired);
        drop(state);
        drop(
            solaris_agent::testing::request(Arc::clone(&backend), request())
                .await
                .expect("second prompt"),
        );
        assert_eq!(*calls.lock().expect("calls"), vec!["fresh-1", "fresh-2"]);
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn rejected_refresh_never_sends_a_model_request_or_changes_saved_credentials() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("auth.json");
        let (shared, calls) = setup(
            path.clone(),
            Arc::new(|_| Box::pin(async { Err("refresh rejected".into()) })),
        );
        shared.initial.save(&path).expect("original credentials");
        let before = std::fs::read(&path).expect("before");
        let result =
            solaris_agent::testing::request(shared.choice("model").backend, request()).await;
        assert!(matches!(result, Err(error) if error.to_string().contains("refresh rejected")));
        assert!(calls.lock().expect("calls").is_empty());
        assert_eq!(std::fs::read(path).expect("after"), before);
    }

    #[tokio::test]
    async fn failed_persistence_blocks_requests_and_retries_saving_the_rotated_token() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("auth.json");
        std::fs::create_dir(&path).expect("block replacement");
        let count = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&count);
        let (shared, calls) = setup(
            path.clone(),
            Arc::new(move |_| {
                observed.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(credential("rotated", u64::MAX)) })
            }),
        );
        let backend = shared.choice("model").backend;
        assert!(
            solaris_agent::testing::request(Arc::clone(&backend), request())
                .await
                .is_err()
        );
        assert!(calls.lock().expect("calls").is_empty());
        std::fs::remove_dir(&path).expect("restore storage");
        drop(
            solaris_agent::testing::request(Arc::clone(&backend), request())
                .await
                .expect("retry saved token"),
        );
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(*calls.lock().expect("calls"), vec!["rotated"]);
        assert!(AuthStore::from_json(&std::fs::read_to_string(path).expect("saved")).is_ok());
    }

    #[tokio::test]
    async fn unexpired_credentials_do_not_refresh_or_require_an_auth_path() {
        let directory = tempfile::tempdir().expect("directory");
        let (mut shared, calls) = setup(
            directory.path().join("auth.json"),
            Arc::new(|_| panic!("fresh credentials must not refresh")),
        );
        let owned = Arc::get_mut(&mut shared).expect("sole owner");
        owned.initial.store("openai", credential("valid", u64::MAX));
        owned.current.get_mut().auth = owned.initial.clone();
        owned.path = None;
        drop(
            solaris_agent::testing::request(shared.choice("model").backend, request())
                .await
                .expect("valid request"),
        );
        assert_eq!(*calls.lock().expect("calls"), vec!["valid"]);
    }
}
