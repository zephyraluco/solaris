//! Model execution and business contracts.
//!
//! Public messages, tools, events and usage are independent of the SDK. The
//! single production [`Backend`] delegates protocol encoding and stream decoding
//! to the official model SDK; authentication and model resolution belong to the
//! provider layer, while conversation and execution policy belong to the agent.

mod client;
mod connection;
mod discovery;
pub use connection::ModelClient;
mod convert;
mod event;
mod http;
mod message;
mod policy;
mod request;
mod tool;
mod transport;
mod usage;
mod wire;

pub use client::{Backend, Endpoint, TurnPlan};
pub use event::{AgentEvent, BackendError};
pub use http::IDLE_TIMEOUT;
pub use message::{Content, Message, MessageContext, Role};
pub use tool::{ToolCall, ToolResult, ToolSpec};
pub use usage::{Usage, tokens_for_characters};
pub use wire::Wire;

use std::pin::Pin;

use futures::Stream;

/// Boxed, `Send` stream of events produced by one turn.
pub type AgentEventStream = Pin<Box<dyn Stream<Item = AgentEvent> + Send>>;

/// Prepares Rig model connections and exposes connection metadata.
///
/// The provider resolves routing and credentials; policy wrappers prepare the
/// same Rig connection and attach accounting, budgets, or authentication refresh.
#[async_trait::async_trait]
pub trait AgentBackend: Send + Sync {
    /// Prepare one immutable Rig model connection.
    async fn model_client(self: std::sync::Arc<Self>) -> Result<ModelClient, BackendError>;

    /// Explicit model metadata, with unknown values kept distinct from refusal.
    fn capabilities(&self) -> solaris_types::ModelCapabilities {
        solaris_types::ModelCapabilities::default()
    }
    /// Explains a stale/offline model-list fallback to the caller.
    fn model_catalog_notice(&self) -> Option<String> {
        None
    }

    /// Short human-readable label shown in the status bar.
    fn label(&self) -> &str {
        "backend"
    }

    /// The models the endpoint offers, for the picker to choose from.
    ///
    /// Best-effort, and the reason this returns a `Result` rather than a list:
    /// a caller with its own catalogue wants to fall back to that, so "this
    /// backend cannot ask" has to stay distinguishable from "nothing is offered".
    async fn models(&self) -> Result<Vec<String>, BackendError> {
        Err(BackendError::new(
            "this backend cannot list models — the catalogue is all there is",
        ))
    }
}

pub use solaris_types::{ContextUsage, ModelCapabilities, Support};

pub use solaris_types::InlineImage;

#[cfg(feature = "test-utils")]
pub mod testing;
