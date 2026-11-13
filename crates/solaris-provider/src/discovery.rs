//! Endpoint model discovery and offline model-list cache.
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use solaris_backend::{Backend, BackendError};
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

pub(crate) struct ModelDiscovery {
    pub inner: Arc<Backend>,
    pub cache_dir: Option<PathBuf>,
    pub endpoint: String,
    pub notice: Mutex<Option<String>>,
}
impl ModelDiscovery {
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
/// Endpoint discovery is separate from model execution.
#[async_trait]
pub trait ModelSource: Send + Sync {
    async fn models(&self) -> Result<Vec<String>, BackendError>;
    fn notice(&self) -> Option<String> {
        None
    }
}

pub struct UnavailableModels;
#[async_trait]
impl ModelSource for UnavailableModels {
    async fn models(&self) -> Result<Vec<String>, BackendError> {
        Err(BackendError::new("model discovery is unavailable"))
    }
}

#[async_trait]
impl ModelSource for ModelDiscovery {
    fn notice(&self) -> Option<String> {
        self.notice
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
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
