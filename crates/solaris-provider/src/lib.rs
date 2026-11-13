//! Provider configuration and model selection, with official catalog and authentication.
//!
//! The application assembles these services; the agent depends only on the backend.
//! Connection settings choose routing and credentials. Official model facts supply
//! capabilities, limits, encoding rules and reference pricing.

mod credentials;
pub mod oauth;
pub mod providers;
mod selection;
pub use credentials::refreshing_backend_factory;

pub use selection::{
    BackendChoice, BackendOptions, CredentialSource, Environment, choose_backend,
    empty_environment, process_environment,
};
pub use solaris_backend::Wire;

pub use providers::{
    AuthKind, DEFAULT_CONTEXT_WINDOW, DEFAULT_MAX_OUTPUT, PROVIDERS, ProviderSpec, provider_spec,
};

pub use credentials::{Active, AuthError, AuthStore, Credential, mask_secret};
mod config;
pub use config::{ModelSelection, Protocol, ProviderConfig, ProviderEndpoint, ResolvedModel};
mod discovery;

pub use discovery::{ModelSource, UnavailableModels};
