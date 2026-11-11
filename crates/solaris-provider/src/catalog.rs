//! Provider-scoped model metadata and endpoint model-list caching.
mod cache;
mod resolve;
mod schema;
mod sources;
#[cfg(test)]
mod tests;

pub use resolve::{CatalogReference, ModelCatalog, ModelOverride, ResolvedModel};
pub use schema::{CatalogModel, CatalogSnapshot, ParameterSources, SNAPSHOT_PROVIDERS};
pub use sources::{MODELS_DEV_URL, OPENROUTER_URL, parse_models_dev, parse_openrouter};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use solaris_backend::{AgentBackend, AgentEventStream, BackendError, TurnRequest};
use solaris_types::ModelCapabilities;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Serialize, Deserialize)]
struct CachedModels {
    version: u32,
    endpoint: String,
    ids: Vec<String>,
    checksum: String,
}
fn digest(text: &[u8]) -> String {
    format!("{:x}", Sha256::digest(text))
}

/// A fresh immutable parameter set is taken at the start of each model request.
pub(crate) struct LiveModel {
    pub catalog: Arc<ModelCatalog>,
    pub provider: &'static str,
    pub model: String,
    pub prototype: solaris_backend::Backend,
    pub account_authorized: bool,
}
impl LiveModel {
    fn resolved(&self) -> ResolvedModel {
        let mut resolved = self.catalog.resolve(self.provider, &self.model);
        if self.account_authorized {
            resolved.price = None;
            resolved.price_tiers.clear();
            resolved.capabilities.price_known = Some(false);
        }
        resolved
    }
    fn backend(&self) -> solaris_backend::Backend {
        let resolved = self.resolved();
        self.prototype.clone().with_model_parameters(
            resolved.request_id,
            resolved.output_tokens,
            resolved.capabilities,
            resolved.price,
            resolved.price_tiers,
        )
    }
}

pub(crate) fn output_budget(capabilities: &ModelCapabilities, requested: u32) -> u32 {
    capabilities
        .max_output
        .unwrap_or(crate::DEFAULT_MAX_OUTPUT)
        .min(requested)
        .min(
            capabilities
                .context_window
                .unwrap_or(crate::DEFAULT_CONTEXT_WINDOW)
                .saturating_sub(1)
                .max(1),
        )
}

pub(crate) struct CatalogBackend {
    pub inner: Arc<dyn AgentBackend>,
    pub live: Option<LiveModel>,
    pub capabilities: ModelCapabilities,
    pub cache_dir: Option<PathBuf>,
    pub endpoint: String,
    pub notice: Mutex<Option<String>>,
}
impl CatalogBackend {
    fn cache_path(&self) -> Option<PathBuf> {
        self.cache_dir
            .as_ref()
            .map(|dir| dir.join(format!("{}.json", digest(self.endpoint.as_bytes()))))
    }
    fn save(&self, ids: &[String]) -> Result<(), String> {
        let Some(path) = self.cache_path() else {
            return Ok(());
        };
        let directory = path.parent().ok_or("invalid cache directory")?;
        std::fs::create_dir_all(directory).map_err(|error| error.to_string())?;
        let ids_json = serde_json::to_vec(ids).map_err(|error| error.to_string())?;
        let data = CachedModels {
            version: 1,
            endpoint: digest(self.endpoint.as_bytes()),
            ids: ids.to_vec(),
            checksum: digest(&ids_json),
        };
        let mut file =
            tempfile::NamedTempFile::new_in(directory).map_err(|error| error.to_string())?;
        use std::io::Write;
        file.write_all(&serde_json::to_vec(&data).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?;
        file.persist(path).map_err(|error| error.to_string())?;
        Ok(())
    }
    fn cached(&self) -> Option<Vec<String>> {
        let path = self.cache_path()?;
        if std::fs::metadata(&path).ok()?.len() > 1024 * 1024 {
            return None;
        }
        let data: CachedModels = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
        if data.version != 1
            || data.endpoint != digest(self.endpoint.as_bytes())
            || digest(&serde_json::to_vec(&data.ids).ok()?) != data.checksum
        {
            return None;
        }
        Some(data.ids)
    }
}
#[async_trait]
impl AgentBackend for CatalogBackend {
    fn label(&self) -> &str {
        self.inner.label()
    }
    fn capabilities(&self) -> ModelCapabilities {
        self.live.as_ref().map_or_else(
            || self.capabilities.clone(),
            |live| live.resolved().capabilities,
        )
    }
    fn model_catalog_notice(&self) -> Option<String> {
        self.notice
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    async fn run_turn(&self, request: TurnRequest) -> Result<AgentEventStream, BackendError> {
        match &self.live {
            Some(live) => live.backend().run_turn(request).await,
            None => self.inner.run_turn(request).await,
        }
    }
    async fn models(&self) -> Result<Vec<String>, BackendError> {
        match self.inner.models().await {
            Ok(ids) => {
                let warning = self.save(&ids).err().map(|_| {
                    "Model discovery succeeded, but its local cache could not be saved.".to_string()
                });
                *self.notice.lock().unwrap_or_else(|p| p.into_inner()) = warning;
                Ok(ids)
            }
            Err(error) => {
                match self.cached().filter(|ids| !ids.is_empty()) {
                    Some(ids) => {
                        *self.notice.lock().unwrap_or_else(|p| p.into_inner()) = Some("Using a cached model list; remote discovery failed. Retry to refresh it.".into());
                        Ok(ids)
                    }
                    None => Err(error),
                }
            }
        }
    }
}

#[cfg(test)]
mod model_list_tests {
    use super::*;
    use solaris_types::Support;
    #[test]
    fn overrides_are_provider_scoped_and_unknown_features_stay_unknown() {
        let catalog = ModelCatalog::from_json(r#"{"models":[{"provider":"local","id":"custom","capabilities":{"tool_calling":"unsupported","context_window":8192}}]}"#).expect("catalog");
        assert_eq!(
            catalog.capabilities("local", "custom").tool_calling,
            Support::Unsupported
        );
        assert_eq!(
            catalog.capabilities("other", "custom").tool_calling,
            Support::Unknown
        );
        assert_eq!(
            catalog.capabilities("local", "custom").price_known,
            Some(false)
        );
    }
    #[test]
    fn invalid_limits_and_duplicate_ids_are_rejected() {
        assert!(ModelCatalog::from_json(r#"{"models":[{"provider":"local","id":"custom","capabilities":{"context_window":0}}]}"#).is_err());
    }
    #[tokio::test]
    async fn cache_corruption_is_not_treated_as_a_valid_model_list() {
        struct Missing;
        #[async_trait]
        impl AgentBackend for Missing {
            async fn run_turn(&self, _: TurnRequest) -> Result<AgentEventStream, BackendError> {
                Err(BackendError::new("unused"))
            }
        }
        let directory = tempfile::tempdir().expect("directory");
        let backend = CatalogBackend {
            inner: Arc::new(Missing),
            live: None,
            capabilities: ModelCapabilities::default(),
            cache_dir: Some(directory.path().into()),
            endpoint: "https://example.com/v1".into(),
            notice: Mutex::new(None),
        };
        backend.save(&["one".into()]).expect("save");
        assert_eq!(backend.cached(), Some(vec!["one".into()]));
        assert_eq!(
            backend.models().await.expect("cached fallback"),
            vec!["one"]
        );
        assert!(
            backend
                .model_catalog_notice()
                .expect("notice")
                .contains("cached")
        );
        let path = backend.cache_path().expect("path");
        let mut data: Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read")).expect("json");
        data["ids"] = serde_json::json!(["tampered"]);
        std::fs::write(path, serde_json::to_vec(&data).expect("json")).expect("tamper");
        assert_eq!(backend.cached(), None);
    }
    use serde_json::Value;
}
