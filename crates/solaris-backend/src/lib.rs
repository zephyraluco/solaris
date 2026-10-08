//! Backend abstraction for solaris.
//!
//! One client serves every platform: it is handed an endpoint, a model, and
//! whatever the caller knows about pricing, and it streams a turn back over
//! whichever wire that endpoint speaks. **Which platform an endpoint belongs to,
//! how its credentials were found, and which models it offers are all the
//! caller's business** — that is `solaris-provider`'s job, and this crate never
//! learns any of it.

mod client;
mod http;
mod protocols;
mod sse;
mod wire;

pub use client::{Endpoint, HttpBackend, TurnPlan};
pub use http::IDLE_TIMEOUT;
pub use wire::Wire;

use std::pin::Pin;

use futures::Stream;
use solaris_core::{AgentEvent, BackendError, TurnRequest};

/// Boxed, `Send` stream of events produced by one turn.
pub type AgentEventStream = Pin<Box<dyn Stream<Item = AgentEvent> + Send>>;

/// Produces a stream of [`AgentEvent`]s for a turn.
///
/// The uniform contract every backend implements. `solaris-provider` resolves a
/// platform into one of these, and the UI only ever sees this.
#[async_trait::async_trait]
pub trait AgentBackend: Send + Sync {
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
