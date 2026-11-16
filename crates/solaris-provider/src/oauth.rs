//! Browser login and refresh delegated to the official provider authenticator.
use crate::Credential;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Failures never include callback URLs, authorization codes or token bodies.
#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("could not start OpenAI login: {0}")]
    Io(#[from] std::io::Error),
    #[error("OpenAI login timed out; run /connect to retry")]
    Timeout,
    #[error("OpenAI authorization expired; run /connect again")]
    SignInRequired,
    #[error("OpenAI token request failed; check your network and run /connect again")]
    Transport,
    #[error("OpenAI rejected the token request (HTTP {0}); run /connect again")]
    Rejected(u16),
    #[error("OpenAI returned invalid or incomplete credentials")]
    InvalidTokens,
}

/// Run the official browser flow. Dropping the future cancels its listener.
/// The callback receives the authorization URL after a callback port is bound.
pub async fn login(show_url: impl FnOnce(String) + Send) -> Result<Credential, OAuthError> {
    tokio::time::timeout(
        Duration::from_secs(300),
        crate::credentials::login(show_url),
    )
    .await
    .map_err(|_| OAuthError::Timeout)?
}

/// Current Unix time for persisted credential expiry.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |time| time.as_secs())
}

/// Refresh before the next request, never during an in-flight model call.
pub fn needs_refresh(credential: &Credential) -> bool {
    matches!(credential, Credential::ChatGpt { expires_at, .. } if *expires_at <= now().saturating_add(60))
}

/// Refresh only through the official authenticator's fixed issuer.
pub async fn refresh(credential: &Credential) -> Result<Credential, OAuthError> {
    crate::credentials::refresh(credential).await
}
