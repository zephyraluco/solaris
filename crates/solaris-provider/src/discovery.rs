//! Endpoint model discovery and offline model-list cache.
use crate::{ModelCatalog, ResolvedModel};
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
            if let Some(spec) = resolved.facts.spec() {
                let mut spec = spec.clone();
                spec.pricing = None;
                resolved.facts = rig_core::catalog::ModelFacts::new(spec)
                    .with_catalog(&rig_core::catalog::Catalog::default());
            }
            resolved.capabilities.price_known = Some(false);
        }
        resolved
    }
    fn backend(&self) -> solaris_backend::Backend {
        let resolved = self.resolved();
        let wire = if self.account_authorized {
            solaris_backend::Wire::OpenAiResponses
        } else {
            crate::provider_spec(self.provider)
                .map(|provider| provider.wire_for(&resolved.request_id))
                .unwrap_or_else(|| self.prototype.wire())
        };
        self.prototype.clone().with_wire(wire).with_model_facts(
            resolved.request_id,
            resolved.output_tokens,
            resolved.facts,
            resolved.capabilities,
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
    async fn model_client(self: Arc<Self>) -> Result<solaris_backend::ModelClient, BackendError> {
        match &self.live {
            Some(live) => Arc::new(live.backend()).model_client().await,
            None => Arc::clone(&self.inner).model_client().await,
        }
    }

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
