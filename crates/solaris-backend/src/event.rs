use thiserror::Error;

/// Errors a backend can fail with before/while streaming.
#[derive(Debug, Error)]
pub enum BackendError {
    #[error("{0}")]
    Message(String),
}

impl BackendError {
    pub fn new(message: impl Into<String>) -> Self {
        BackendError::Message(message.into())
    }
}
