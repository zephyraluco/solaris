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
    saved_credential(&store, Some(credential)).await
}

pub(crate) async fn login(show_url: impl FnOnce(String) + Send) -> Result<Credential, OAuthError> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|_| OAuthError::Transport)?;
    login_with(&rig_reqwest::ReqwestClient::from(client), show_url).await
}

async fn login_with<H: HttpClientExt>(
    http: &H,
    show_url: impl FnOnce(String) + Send,
) -> Result<Credential, OAuthError> {
    let store = Arc::new(MemoryTokenStore::default());
    let auth = Authenticator::new(
        AuthSource::OAuth,
        Arc::clone(&store),
        DeviceCodeHandler::default(),
        false,
    );
    // The official SDK owns PKCE, state validation, callback listener and code
    // exchange. The application only displays the URL and persists the result.
    auth.sign_in_with_browser(http, |page| show_url(page.authorize_url))
        .await
        .map_err(authentication_error)?;
    saved_credential(&store, None).await
}

async fn saved_credential(
    store: &MemoryTokenStore,
    previous: Option<&Credential>,
) -> Result<Credential, OAuthError> {
    let saved = store
        .load("chatgpt")
        .await
        .map_err(|_| OAuthError::InvalidTokens)?
        .ok_or(OAuthError::InvalidTokens)?;
    let record: TokenRecord =
        serde_json::from_str(&saved).map_err(|_| OAuthError::InvalidTokens)?;
    let (old_refresh, old_id, old_account) = match previous {
        Some(Credential::ChatGpt {
            refresh_token,
            id_token,
            account_id,
            ..
        }) => (
            refresh_token.as_str(),
            id_token.as_str(),
            account_id.as_str(),
        ),
        _ => ("", "", ""),
    };
    let expires_at = record
        .expires_at
        .filter(|value| *value > 0)
        .ok_or(OAuthError::InvalidTokens)?;
    let account_id = record
        .account_id
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| old_account.into());
    let refresh_token = record
        .refresh_token
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| old_refresh.into());
    if record.access_token.is_empty() || account_id.is_empty() || refresh_token.is_empty() {
        return Err(OAuthError::InvalidTokens);
    }
    Ok(Credential::ChatGpt {
        access_token: record.access_token,
        refresh_token,
        id_token: record.id_token.unwrap_or_else(|| old_id.into()),
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

#[cfg(test)]
mod browser_tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use rig_core::test_utils::RecordingHttpClient;
    use std::time::Duration;

    #[tokio::test]
    async fn official_browser_login_exchanges_code_and_cancellation_releases_listener() {
        let access = format!("e30.{}.signature", URL_SAFE_NO_PAD.encode(br#"{"exp":4000000000,"https://api.openai.com/auth":{"chatgpt_account_id":"account-123"}}"#));
        let http = RecordingHttpClient::new(serde_json::json!({"access_token":access,"refresh_token":"refresh-secret","id_token":"id-secret"}).to_string());
        let observed = http.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            login_with(&http, |url| {
                let _ = tx.send(url);
            })
            .await
        });
        let url = tokio::time::timeout(Duration::from_secs(10), rx)
            .await
            .unwrap()
            .unwrap();
        let url = url::Url::parse(&url).unwrap();
        assert_eq!(url.host_str(), Some("auth.openai.com"));
        let fields: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(fields["code_challenge_method"], "S256");
        let mut callback = url::Url::parse(&fields["redirect_uri"]).unwrap();
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        callback.set_query(Some("code=forged&state=wrong"));
        assert_eq!(
            client.get(callback.clone()).send().await.unwrap().status(),
            400
        );
        assert!(observed.requests().is_empty());
        callback
            .query_pairs_mut()
            .clear()
            .append_pair("code", "test-code")
            .append_pair("state", &fields["state"]);
        let page = client
            .get(callback.clone())
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(!page.contains("refresh-secret"));
        let result = task.await.unwrap().unwrap();
        assert!(
            matches!(result, Credential::ChatGpt { account_id, refresh_token, expires_at: 4_000_000_000, .. } if account_id == "account-123" && refresh_token == "refresh-secret")
        );
        let requests = observed.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].uri, "https://auth.openai.com/oauth/token");
        // Start a second official flow and abort while it waits for the browser.
        let (tx, rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            login_with(&RecordingHttpClient::new("{}"), |url| {
                let _ = tx.send(url);
            })
            .await
        });
        let url = url::Url::parse(&rx.await.unwrap()).unwrap();
        let redirect = url
            .query_pairs()
            .find(|(key, _)| key == "redirect_uri")
            .unwrap()
            .1
            .into_owned();
        let redirect = url::Url::parse(&redirect).unwrap();
        let port = redirect.port().unwrap();
        task.abort();
        let _ = task.await;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("SDK listener should stop after cancellation");
    }
}
