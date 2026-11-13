//! Official model metadata with application-owned loading and gateway identity rules.
mod cache;
mod resolve;
pub use resolve::{ModelCatalog, ModelSelection, ResolvedModel};
pub const MODELS_DEV_URL: &str = "https://models.dev/api.json";
