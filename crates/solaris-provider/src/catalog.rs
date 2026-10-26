//! Built-in metadata with explicit local overrides and integrity-checked model-list caches.
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use solaris_backend::{AgentBackend, AgentEventStream, BackendError, Price, TurnRequest};
use solaris_types::{ModelCapabilities, Support};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCatalog {
    #[serde(default)]
    pub models: Vec<ModelOverride>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelOverride {
    pub provider: String,
    pub id: String,
    #[serde(default)]
    pub capabilities: ModelCapabilities,
    #[serde(default)]
    pub price: Option<Price>,
}
impl ModelCatalog {
    pub fn from_json(text: &str) -> Result<Self, String> {
        if text.len() > 1024 * 1024 {
            return Err("model catalog exceeds 1 MiB".into());
        }
        let catalog: Self = serde_json::from_str(text).map_err(|error| error.to_string())?;
        let mut ids = BTreeSet::new();
        for entry in &catalog.models {
            if entry.id.is_empty()
                || entry.id.len() > 512
                || !ids.insert((&entry.provider, &entry.id))
            {
                return Err("empty or duplicate provider/model ID".into());
            }
            if entry.capabilities.context_window == Some(0)
                || entry.capabilities.max_output == Some(0)
            {
                return Err("model token limits must be positive".into());
            }
            if let Some(price) = entry.price
                && [
                    price.input,
                    price.output,
                    price.cache_read,
                    price.cache_write,
                ]
                .iter()
                .any(|rate| !rate.is_finite() || *rate < 0.0)
            {
                return Err("model prices must be finite and nonnegative".into());
            }
        }
        Ok(catalog)
    }
    pub fn capabilities(&self, provider: &str, model: &str) -> ModelCapabilities {
        let mut capabilities = self
            .models
            .iter()
            .find(|entry| entry.provider == provider && entry.id == model)
            .map(|entry| entry.capabilities.clone())
            .unwrap_or_default();
        let spec = crate::model_spec(model);
        if capabilities.tool_calling == Support::Unknown && spec.is_some() {
            capabilities.tool_calling = Support::Supported;
        }
        capabilities.context_window = capabilities
            .context_window
            .or_else(|| spec.and_then(|spec| spec.context_window));
        capabilities.max_output = capabilities
            .max_output
            .or_else(|| spec.and_then(|spec| spec.max_output));
        capabilities.price_known = Some(self.price(provider, model).is_some());
        capabilities
    }
    pub fn price(&self, provider: &str, model: &str) -> Option<Price> {
        self.models
            .iter()
            .find(|entry| entry.provider == provider && entry.id == model)
            .and_then(|entry| entry.price)
            .or_else(|| crate::price_for(model))
    }
}

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

pub(crate) struct CatalogBackend {
    pub inner: Arc<dyn AgentBackend>,
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
        self.capabilities.clone()
    }
    fn model_catalog_notice(&self) -> Option<String> {
        self.notice
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    async fn run_turn(&self, request: TurnRequest) -> Result<AgentEventStream, BackendError> {
        self.inner.run_turn(request).await
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
mod tests {
    use super::*;
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
