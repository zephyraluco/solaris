//! Lifecycle of the official model catalog; no parallel model schema or parser.
mod cache;
use rig_core::catalog::Catalog;
use std::{
    path::Path,
    sync::{Arc, RwLock},
};
pub const MODELS_DEV_URL: &str = "https://models.dev/api.json";
#[derive(Debug, Clone)]
pub struct ModelCatalog {
    pub(crate) state: Arc<RwLock<Catalog>>,
    pub(crate) overrides: Arc<String>,
}
impl Default for ModelCatalog {
    fn default() -> Self {
        Self::new(Catalog::builtin().clone(), "{}".into())
    }
}
impl ModelCatalog {
    pub(crate) fn new(catalog: Catalog, overrides: String) -> Self {
        Self {
            state: Arc::new(RwLock::new(catalog)),
            overrides: Arc::new(overrides),
        }
    }
    pub fn load_cache(&self, directory: &Path) {
        cache::load(self, directory);
    }
    pub fn start_refresh(&self, directory: std::path::PathBuf) {
        cache::start(self.clone(), directory);
    }
}
