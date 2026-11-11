//! Adapt the application's credential format to the SDK's OAuth refresh flow.
use std::sync::Arc;

use rig_core::http_client::HttpClientExt;
use rig_core::providers::chatgpt::auth::{
    AuthError, AuthSource, Authenticator, DeviceCodeHandler, MemoryTokenStore, TokenStore,
};
use serde::{Deserialize, Serialize};

use crate::{Credential, oauth::OAuthError};

#[derive(Serialize, Deserialize)]
struct TokenRecord {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_at: Option<u64>,
    account_id: Option<String>,
}

pub(crate) async fn refresh(credential: &Credential) -> Result<Credential, OAuthError> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|_| OAuthError::Transport)?;
    refresh_with(credential, &rig_reqwest::ReqwestClient::from(client)).await
}

async fn refresh_with<H: HttpClientExt>(
    credential: &Credential,
    http: &H,
) -> Result<Credential, OAuthError> {
    let Credential::ChatGpt {
        access_token,
        refresh_token,
        id_token,
        expires_at,
        account_id,
    } = credential
    else {
        return Err(OAuthError::InvalidTokens);
    };
    let store = Arc::new(MemoryTokenStore::default());
    let record = TokenRecord {
        access_token: access_token.clone(),
        refresh_token: Some(refresh_token.clone()),
        id_token: Some(id_token.clone()),
        expires_at: Some(*expires_at),
        account_id: Some(account_id.clone()),
    };
    store
        .save(
            "chatgpt",
            &serde_json::to_string(&record).map_err(|_| OAuthError::InvalidTokens)?,
        )
        .await
        .map_err(|_| OAuthError::InvalidTokens)?;
    // Never start an interactive login while a model request waits for refresh.
    // The SDK owns issuer selection, expiry checks and the refresh exchange.
    let auth = Authenticator::new(
        AuthSource::OAuth,
        Arc::clone(&store),
        DeviceCodeHandler::default(),
        false,
    );
    auth.auth_context(http)
        .await
        .map_err(authentication_error)?;
    let saved = store
        .load("chatgpt")
        .await
        .map_err(|_| OAuthError::InvalidTokens)?
        .ok_or(OAuthError::InvalidTokens)?;
    let record: TokenRecord =
        serde_json::from_str(&saved).map_err(|_| OAuthError::InvalidTokens)?;
    let expires_at = record
        .expires_at
        .filter(|value| *value > 0)
        .ok_or(OAuthError::InvalidTokens)?;
    let account_id = record
        .account_id
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| account_id.clone());
    let refresh_token = record
        .refresh_token
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| refresh_token.clone());
    if record.access_token.is_empty() || account_id.is_empty() || refresh_token.is_empty() {
        return Err(OAuthError::InvalidTokens);
    }
    Ok(Credential::ChatGpt {
        access_token: record.access_token,
        refresh_token,
        id_token: record.id_token.unwrap_or_else(|| id_token.clone()),
        expires_at,
        account_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use rig_core::test_utils::RecordingHttpClient;

    fn credential() -> Credential {
        Credential::ChatGpt {
            access_token: "expired".into(),
            refresh_token: "old-refresh".into(),
            id_token: "old-id".into(),
            expires_at: 0,
            account_id: "old-account".into(),
        }
    }
    fn token(exp: u64) -> String {
        format!(
            "e30.{}.signature",
            URL_SAFE_NO_PAD.encode(serde_json::json!({"exp":exp}).to_string())
        )
    }
    #[tokio::test]
    async fn sdk_refresh_uses_fixed_issuer_and_preserves_optional_fields() {
        let access = token(u64::MAX / 2);
        let http = RecordingHttpClient::new(serde_json::json!({"access_token":access}).to_string());
        let refreshed = refresh_with(&credential(), &http).await.unwrap();
        assert!(
            matches!(refreshed, Credential::ChatGpt { access_token, refresh_token, id_token, account_id, expires_at }
            if access_token == access && refresh_token == "old-refresh" && id_token == "old-id" && account_id == "old-account" && expires_at == u64::MAX / 2)
        );
        let requests = http.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].uri, "https://auth.openai.com/oauth/token");
    }
    #[tokio::test]
    async fn sdk_refresh_keeps_rotated_refresh_token() {
        let http = RecordingHttpClient::new(
            serde_json::json!({"access_token":token(4_000_000_000),"refresh_token":"rotated"})
                .to_string(),
        );
        let refreshed = refresh_with(&credential(), &http).await.unwrap();
        assert!(
            matches!(refreshed, Credential::ChatGpt { refresh_token, .. } if refresh_token == "rotated")
        );
    }
    #[tokio::test]
    async fn api_keys_are_not_sent_to_oauth() {
        let http = RecordingHttpClient::new("{}");
        let result = refresh_with(
            &Credential::ApiKey {
                key: "secret".into(),
            },
            &http,
        )
        .await;
        assert!(matches!(result, Err(OAuthError::InvalidTokens)));
        assert!(http.requests().is_empty());
    }
}

// SDK error strings may contain issuer response bodies. Keep the application's
// existing safe errors instead of forwarding arbitrary OAuth text to the UI.
fn authentication_error(error: AuthError) -> OAuthError {
    match error {
        AuthError::Message(message) if message.contains("sign-in required") => {
            OAuthError::SignInRequired
        }
        AuthError::Http(error) => error
            .non_success_status()
            .map_or(OAuthError::Transport, |status| {
                OAuthError::Rejected(status.as_u16())
            }),
        AuthError::Json(_) | AuthError::Store(_) => OAuthError::InvalidTokens,
        _ => OAuthError::Transport,
    }
}

#[cfg(test)]
mod failure_tests {
    use super::*;
    #[test]
    fn issuer_error_bodies_are_not_displayed() {
        let error = authentication_error(AuthError::Message("secret-token issuer failure".into()));
        assert!(!error.to_string().contains("secret-token"));
        assert!(matches!(
            authentication_error(AuthError::Message("ChatGPT sign-in required".into())),
            OAuthError::SignInRequired
        ));
    }
}
