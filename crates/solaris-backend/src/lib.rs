//! Native model connections and request policy.
//!
//! Connection errors and request usage are independent of session events. The
//! single production [`Backend`] delegates protocol encoding and stream decoding
//! to the official model SDK; authentication and model resolution belong to the
//! provider layer, while conversation and execution policy belong to the agent.

mod client;
mod parameters;
pub use parameters::{DEFAULT_CONTEXT_WINDOW, DEFAULT_MAX_OUTPUT, ModelParameters};
mod connection;
mod discovery;
pub use connection::ModelClient;
mod convert;
mod error;
mod http;
mod message;
mod policy;
mod request;
mod transport;
mod usage;
mod wire;

pub use client::{Backend, Endpoint, TurnPlan};
pub use error::BackendError;
pub use http::IDLE_TIMEOUT;
pub use message::{Content, Message, Role};
use solaris_types::{ToolCall, ToolResult};
pub use usage::{Usage, tokens_for_characters};
pub use wire::Wire;

/// Prepares Rig model connections and exposes connection metadata.
///
/// The provider resolves routing and credentials; policy wrappers prepare the
/// same Rig connection and attach accounting, budgets, or authentication refresh.
#[async_trait::async_trait]
pub trait ModelConnector: Send + Sync {
    /// Prepare one immutable Rig model connection.
    async fn model_client(self: std::sync::Arc<Self>) -> Result<ModelClient, BackendError>;

    /// Explicit model metadata, with unknown values kept distinct from refusal.
    fn capabilities(&self) -> solaris_types::ModelCapabilities {
        solaris_types::ModelCapabilities::default()
    }
    /// Short human-readable label shown in the status bar.
    fn label(&self) -> &str {
        "backend"
    }
}

pub use solaris_types::{ContextUsage, ModelCapabilities, Support};

pub use solaris_types::InlineImage;

pub use rig_core::completion::Cost;
