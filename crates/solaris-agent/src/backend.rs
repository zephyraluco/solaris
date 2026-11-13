//! Resolved execution inputs supplied by the application.

use std::sync::Arc;

use solaris_backend::ModelConnector;

/// A backend and the application-resolved metadata needed to execute a session.
/// Provider selection, authentication and catalogue lookups belong to the caller.
#[derive(Clone)]
pub struct ConnectionConfig {
    /// Resolved transport, including any caller-owned authentication refresh.
    pub backend: Arc<dyn ModelConnector>,
    /// Model identifier selected by the application.
    pub model: String,
    /// Opaque identifier retained in session records, never used for resolution.
    pub provider_id: Option<String>,
    /// Used only when the backend does not report a context window.
    pub fallback_context_window: u32,
    /// Additional instructions resolved by the application for this backend.
    pub instructions: Option<String>,
}
