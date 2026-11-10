//! Stable model contract and persisted conversation vocabulary for Solaris.
//!
//! This crate owns messages, tools, events, capabilities and accounting types.
//! Official Rig integration and HTTP policy live in solaris-rig; the agent does
//! not depend on a provider SDK or on provider configuration.

mod event;
mod message;
mod tool;
mod usage;
mod wire;

pub use event::{AgentEvent, BackendError, TurnRequest};
pub use message::{Content, Message, MessageContext, Role};
pub use tool::{ToolCall, ToolResult, ToolSpec};
pub use usage::{Price, PriceTier, Usage, tokens_for_characters};
pub use wire::Wire;

use std::pin::Pin;

use futures::Stream;

/// Boxed, `Send` stream of events produced by one turn.
pub type AgentEventStream = Pin<Box<dyn Stream<Item = AgentEvent> + Send>>;

/// Produces a stream of [`AgentEvent`]s for a turn.
///
/// The uniform contract every backend implements. `solaris-provider` resolves a
/// platform into one of these, and the UI only ever sees this.
#[async_trait::async_trait]
pub trait AgentBackend: Send + Sync {
    /// Explicit model metadata, with unknown values kept distinct from refusal.
    fn capabilities(&self) -> solaris_types::ModelCapabilities {
        solaris_types::ModelCapabilities::default()
    }
    /// Explains a stale/offline model-list fallback to the caller.
    fn model_catalog_notice(&self) -> Option<String> {
        None
    }

    /// Start a turn.
    async fn run_turn(&self, request: TurnRequest) -> Result<AgentEventStream, BackendError>;

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
