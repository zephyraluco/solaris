//! Backend abstraction and the conversation vocabulary for solaris.
//!
//! One client serves every platform: it is handed an endpoint, a model, and
//! whatever the caller knows about pricing, and it streams a turn back over
//! whichever wire that endpoint speaks. **Which platform an endpoint belongs to,
//! how its credentials were found, and which models it offers are all the
//! caller's business** — that is `solaris-provider`'s job, and this crate never
//! learns any of it.
//!
//! What it does own is the vocabulary a turn is written in: the messages and
//! their content blocks ([`Message`], [`Content`], [`Role`]), the tool
//! declarations, calls and results the wire carries ([`ToolSpec`], [`ToolCall`],
//! [`ToolResult`]), what a turn consumes and costs ([`Usage`], [`Price`]), and
//! the events a backend streams back ([`AgentEvent`]). Those types are the wire's
//! own shape — every protocol here serialises them — so they live beside the
//! protocols that speak them rather than in a layer of their own.

mod client;
mod event;
mod http;
mod message;
mod protocols;
mod sse;
mod tool;
mod usage;
mod wire;

pub use client::{Endpoint, HttpBackend, TurnPlan};
pub use event::{AgentEvent, BackendError, TurnRequest};
pub use http::IDLE_TIMEOUT;
pub use message::{Content, Message, Role};
pub use tool::{ToolCall, ToolResult, ToolSpec};
pub use usage::{Price, Usage, tokens_for_characters};
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

pub use solaris_types::{ModelCapabilities, Support};
