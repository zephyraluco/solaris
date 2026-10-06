//! Backend abstraction for solaris.
//!
//! The UI never talks to a provider directly: it hands a [`TurnRequest`] to an
//! [`AgentBackend`] and consumes the resulting [`AgentEventStream`]. Swapping in
//! a real provider later only means implementing this trait.

mod mock;

pub use mock::MockBackend;

use std::pin::Pin;

use futures::Stream;
use solaris_core::{AgentEvent, BackendError, TurnRequest};

/// Boxed, `Send` stream of events produced by one turn.
pub type AgentEventStream = Pin<Box<dyn Stream<Item = AgentEvent> + Send>>;

/// Produces a stream of [`AgentEvent`]s for a turn.
#[async_trait::async_trait]
pub trait AgentBackend: Send + Sync {
    /// Start a turn.
    async fn run_turn(&self, request: TurnRequest) -> Result<AgentEventStream, BackendError>;

    /// Short human-readable label shown in the status bar.
    fn label(&self) -> &str {
        "backend"
    }
}
