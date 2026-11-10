//! Official Rig transport adapters behind Solaris's stable backend contract.
mod backend;
mod convert;
mod http;
mod policy;
mod transport;
pub use backend::{Endpoint, RigBackend, TurnPlan};
pub use http::IDLE_TIMEOUT;
