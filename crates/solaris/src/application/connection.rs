//! Connection/account coordination, independent of terminal widgets.
use solaris_provider::{AuthStore, BackendChoice, Credential, ModelSource, ProviderConfig};
use std::{path::PathBuf, sync::Arc};
use tokio::sync::{
    mpsc::{UnboundedReceiver, unbounded_channel},
    oneshot,
};
pub type BackendFactory = Arc<dyn Fn(&AuthStore, &str) -> BackendChoice + Send + Sync>;
pub struct OAuthPending {
    pub url_rx: Option<UnboundedReceiver<String>>,
    pub rx: oneshot::Receiver<Result<Credential, String>>,
    pub(crate) task: tokio::task::JoinHandle<()>,
}
impl Drop for OAuthPending {
    fn drop(&mut self) {
        self.task.abort();
    }
}
pub struct ConnectionController {
    pub backend_factory: BackendFactory,
    pub auth: AuthStore,
    pub auth_base: AuthStore,
    pub auth_path: Option<PathBuf>,
    pub model_source: Arc<dyn ModelSource>,
    pub provider_config: Arc<ProviderConfig>,
    pub discovered: Vec<String>,
    pub models_rx: Option<UnboundedReceiver<Result<Vec<String>, String>>>,
    pub models_error: Option<String>,
    pub discover_models: bool,
    pub oauth: Option<OAuthPending>,
    discovery: Option<tokio::task::AbortHandle>,
}
impl ConnectionController {
    pub fn new(
        backend_factory: BackendFactory,
        auth: AuthStore,
        auth_path: Option<PathBuf>,
        model_source: Arc<dyn ModelSource>,
        provider_config: Arc<ProviderConfig>,
        discover_models: bool,
    ) -> Self {
        Self {
            auth_base: auth.clone(),
            auth,
            auth_path,
            backend_factory,
            model_source,
            provider_config,
            discovered: Vec::new(),
            models_rx: None,
            models_error: None,
            discover_models,
            oauth: None,
            discovery: None,
        }
    }
    pub fn uses_chatgpt(&self) -> bool {
        self.auth.active_provider() == Some("openai")
            && matches!(
                self.auth.credential("openai"),
                Some(Credential::ChatGpt { .. })
            )
    }
    pub fn refresh_models(&mut self) {
        if let Some(task) = self.discovery.take() {
            task.abort();
        }
        self.models_error = None;
        self.models_rx = None;
        self.discovered.clear();
        if !self.discover_models {
            if self.uses_chatgpt() {
                self.models_error = Some("Model discovery is disabled.".into());
            }
            return;
        }
        let models = self.model_source.clone();
        let (tx, rx) = unbounded_channel();
        self.models_rx = Some(rx);
        self.discovery = Some(
            tokio::spawn(async move {
                let _ = tx.send(models.models().await.map_err(|error| error.to_string()));
            })
            .abort_handle(),
        );
    }
    pub fn begin_login(&mut self) {
        self.oauth = None;
        let (url_tx, url_rx) = unbounded_channel();
        let (tx, rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let result = solaris_provider::oauth::login(|url| {
                let _ = url_tx.send(url);
            })
            .await
            .map_err(|error| error.to_string());
            let _ = tx.send(result);
        });
        self.oauth = Some(OAuthPending {
            url_rx: Some(url_rx),
            rx,
            task,
        });
    }
}
impl Drop for ConnectionController {
    fn drop(&mut self) {
        if let Some(task) = self.discovery.take() {
            task.abort();
        }
    }
}
