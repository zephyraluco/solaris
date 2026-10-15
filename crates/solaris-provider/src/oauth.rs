//! OpenAI's Codex/ChatGPT browser authorization (authorization code + PKCE).
//! Protocol reference: openai/codex, codex-rs/login/src/server.rs.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use url::Url;

use crate::Credential;

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const REDIRECT: &str = "http://localhost:1455/auth/callback";

/// Failures never include callback URLs, authorization codes or token bodies.
#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("could not start OpenAI login: {0}")]
    Io(#[from] std::io::Error),
    #[error("could not generate secure OAuth randomness")]
    Random,
    #[error("OpenAI login timed out; run /connect to retry")]
    Timeout,
    #[error("OpenAI authorization was denied or returned no code")]
    Denied,
    #[error("OpenAI token request failed; check your network and run /connect again")]
    Transport,
    #[error("OpenAI rejected the token request (HTTP {0}); run /connect again")]
    Rejected(u16),
    #[error("OpenAI returned invalid or incomplete credentials")]
    InvalidTokens,
}

/// A prepared login owns the loopback port until completed or dropped.
pub struct Login {
    listener: std::net::TcpListener,
    verifier: String,
    state: String,
    url: String,
}

impl Login {
    /// Bind before presenting the URL, so an occupied callback port is visible.
    pub fn bind() -> Result<Self, OAuthError> {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 1455))?;
        listener.set_nonblocking(true)?;
        let verifier = random_secret()?;
        let state = random_secret()?;
        let url = authorization_url(&verifier, &state);
        Ok(Self {
            listener,
            verifier,
            state,
            url,
        })
    }

    /// Browser URL; contains no credential or PKCE verifier.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Wait at most five minutes. Dropping this future releases the port.
    pub async fn complete(self) -> Result<Credential, OAuthError> {
        tokio::time::timeout(Duration::from_secs(300), self.receive(TOKEN_URL))
            .await
            .map_err(|_| OAuthError::Timeout)?
    }

    async fn receive(self, token_url: &str) -> Result<Credential, OAuthError> {
        let listener = TcpListener::from_std(self.listener)?;
        loop {
            let (mut socket, _) = listener.accept().await?;
            let request = tokio::time::timeout(Duration::from_secs(5), async {
                let mut bytes = Vec::new();
                let mut chunk = [0; 1024];
                while bytes.len() < 16_384 {
                    let n = socket.read(&mut chunk).await?;
                    if n == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&chunk[..n]);
                    if bytes.windows(4).any(|s| s == b"\r\n\r\n") {
                        break;
                    }
                }
                Ok::<_, std::io::Error>(bytes)
            })
            .await;
            let Ok(Ok(bytes)) = request else {
                continue;
            };
            let request = String::from_utf8_lossy(&bytes);
            let target = request
                .lines()
                .next()
                .and_then(|line| line.strip_prefix("GET "))
                .and_then(|line| line.strip_suffix(" HTTP/1.1"));
            let result = target.and_then(|target| callback(target, &self.state));
            let Some(result) = result else {
                let _ = socket.write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\nContent-Length: 0\r\n\r\n").await;
                continue;
            };
            let credential = match result {
                Ok(code) => token_request_at(
                    token_url,
                    &[
                        ("grant_type", "authorization_code"),
                        ("code", &code),
                        ("redirect_uri", REDIRECT),
                        ("code_verifier", &self.verifier),
                    ],
                )
                .await
                .and_then(|tokens| tokens.credential(None)),
                Err(error) => Err(error),
            };
            let body = if credential.is_ok() {
                "Authorized. Return to Solaris; you may close this tab."
            } else {
                "Authorization failed. Return to Solaris and retry /connect."
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = tokio::time::timeout(
                Duration::from_secs(5),
                socket.write_all(response.as_bytes()),
            )
            .await;
            return credential;
        }
    }
}

fn random_secret() -> Result<String, OAuthError> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).map_err(|_| OAuthError::Random)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn authorization_url(verifier: &str, state: &str) -> String {
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let query = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs([
            ("response_type", "code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", REDIRECT),
            ("scope", "openid profile email offline_access"),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("state", state),
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
            ("originator", "solaris"),
        ])
        .finish();
    format!("https://auth.openai.com/oauth/authorize?{query}")
}

fn callback(target: &str, state: &str) -> Option<Result<String, OAuthError>> {
    if !target.starts_with('/') || target.starts_with("//") {
        return None;
    }
    let url = Url::parse(&format!("http://localhost:1455{target}")).ok()?;
    if url.path() != "/auth/callback" {
        return None;
    }
    let pairs: Vec<_> = url.query_pairs().collect();
    let values = |key: &str| {
        pairs
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.as_ref())
            .collect::<Vec<_>>()
    };
    if values("state") != [state] {
        return None;
    }
    let codes = values("code");
    Some(
        if !values("error").is_empty() || codes.len() != 1 || codes[0].is_empty() {
            Err(OAuthError::Denied)
        } else {
            Ok(codes[0].to_string())
        },
    )
}

/// Current Unix time, used for token expiry rather than trusting JWT claims.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |t| t.as_secs())
}

/// Whether a ChatGPT credential needs refreshing before another turn.
pub fn needs_refresh(credential: &Credential) -> bool {
    matches!(credential, Credential::ChatGpt { expires_at, .. } if *expires_at <= now().saturating_add(60))
}

/// Exchange only with OpenAI's fixed issuer; never a configurable API endpoint.
pub async fn refresh(credential: &Credential) -> Result<Credential, OAuthError> {
    let Credential::ChatGpt {
        refresh_token,
        id_token,
        account_id,
        ..
    } = credential
    else {
        return Err(OAuthError::InvalidTokens);
    };
    let response = token_request(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
    ])
    .await?;
    response.credential(Some((refresh_token, id_token, account_id)))
}

#[derive(Deserialize)]
struct Tokens {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: u64,
}

impl Tokens {
    fn credential(self, previous: Option<(&str, &str, &str)>) -> Result<Credential, OAuthError> {
        let (old_refresh, old_id, old_account) = previous.unwrap_or_default();
        let refresh_token = self
            .refresh_token
            .unwrap_or_else(|| old_refresh.to_string());
        let id_token = self.id_token.unwrap_or_else(|| old_id.to_string());
        // Claims are metadata only; authentication is performed by OpenAI, not
        // by this unverified local JWT decode.
        let account_id = account_id(&id_token).unwrap_or_else(|| old_account.to_string());
        if self.access_token.is_empty()
            || refresh_token.is_empty()
            || account_id.is_empty()
            || self.expires_in == 0
        {
            return Err(OAuthError::InvalidTokens);
        }
        Ok(Credential::ChatGpt {
            access_token: self.access_token,
            refresh_token,
            id_token,
            expires_at: now().saturating_add(self.expires_in),
            account_id,
        })
    }
}

fn account_id(jwt: &str) -> Option<String> {
    let payload = URL_SAFE_NO_PAD.decode(jwt.split('.').nth(1)?).ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    claims["https://api.openai.com/auth"]["chatgpt_account_id"]
        .as_str()
        .map(str::to_string)
}

async fn token_request(fields: &[(&str, &str)]) -> Result<Tokens, OAuthError> {
    token_request_at(TOKEN_URL, fields).await
}

async fn token_request_at(endpoint: &str, fields: &[(&str, &str)]) -> Result<Tokens, OAuthError> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| OAuthError::Transport)?;
    let mut fields = fields.to_vec();
    fields.push(("client_id", CLIENT_ID));
    let response = client
        .post(endpoint)
        .form(&fields)
        .send()
        .await
        .map_err(|_| OAuthError::Transport)?;
    if !response.status().is_success() {
        return Err(OAuthError::Rejected(response.status().as_u16()));
    }
    response.json().await.map_err(|_| OAuthError::InvalidTokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn browser_callback_exchanges_code_over_http_and_returns_credentials() {
        let callback_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        callback_listener.set_nonblocking(true).unwrap();
        let callback_address = callback_listener.local_addr().unwrap();
        let login = Login {
            listener: callback_listener,
            verifier: "test-verifier".into(),
            state: "expected-state".into(),
            url: String::new(),
        };
        let token_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!(
            "http://{}/oauth/token",
            token_listener.local_addr().unwrap()
        );
        let token_server = tokio::spawn(async move {
            let (mut socket, _) = token_listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0; 1024];
            loop {
                let n = socket.read(&mut chunk).await.unwrap();
                assert_ne!(n, 0);
                bytes.extend_from_slice(&chunk[..n]);
                if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&bytes[..end]);
                    let length: usize = head
                        .lines()
                        .find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse().unwrap())
                        })
                        .unwrap();
                    if bytes.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let request = String::from_utf8(bytes).unwrap();
            assert!(request.contains("code=test-code"));
            assert!(request.contains("code_verifier=test-verifier"));
            assert!(request.contains("grant_type=authorization_code"));
            let claims = URL_SAFE_NO_PAD
                .encode(br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"account-123"}}"#);
            let body = serde_json::json!({ "access_token": "access-secret", "refresh_token": "refresh-secret",
                "id_token": format!("header.{claims}.signature"), "expires_in": 3600 }).to_string();
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(reply.as_bytes()).await.unwrap();
        });
        let task = tokio::spawn(async move { login.receive(&endpoint).await });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        // A forged callback must not stop the server or trigger a token POST.
        let wrong = client
            .get(format!(
                "http://{callback_address}/auth/callback?code=forged&state=wrong"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(wrong.status().as_u16(), 400);
        let browser = client
            .get(format!(
                "http://{callback_address}/auth/callback?code=test-code&state=expected-state"
            ))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(browser.contains("Authorized"));
        assert!(!browser.contains("secret"));
        let credential = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        token_server.await.unwrap();
        assert!(
            matches!(credential, Credential::ChatGpt { account_id, access_token, refresh_token, .. }
            if account_id == "account-123" && access_token == "access-secret" && refresh_token == "refresh-secret")
        );
    }

    #[tokio::test]
    async fn aborting_login_releases_the_callback_port() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let login = Login {
            listener,
            verifier: "v".into(),
            state: "s".into(),
            url: String::new(),
        };
        let task = tokio::spawn(login.complete());
        tokio::task::yield_now().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(std::net::TcpListener::bind(address).is_ok());
    }

    #[test]
    fn pkce_uses_rfc7636_s256_vector() {
        let url = authorization_url("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk", "state");
        assert!(url.contains("code_challenge=E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"));
        assert!(!url.contains("dBjft"));
    }

    #[test]
    fn callback_rejects_missing_mismatched_and_duplicate_state() {
        for target in [
            "/auth/callback?code=x",
            "/auth/callback?code=x&state=wrong",
            "/auth/callback?code=x&state=s&state=s",
            "/other?code=x&state=s",
        ] {
            assert!(callback(target, "s").is_none());
        }
    }

    #[test]
    fn callback_decodes_code_and_reports_denial() {
        assert_eq!(
            callback("/auth/callback?code=a%2Bb&state=s", "s")
                .unwrap()
                .unwrap(),
            "a+b"
        );
        assert!(matches!(
            callback("/auth/callback?error=access_denied&state=s", "s"),
            Some(Err(OAuthError::Denied))
        ));
    }

    #[test]
    fn refresh_keeps_optional_rotating_fields() {
        let token = Tokens {
            access_token: "access".into(),
            refresh_token: None,
            id_token: None,
            expires_in: 3600,
        };
        let credential = token
            .credential(Some(("refresh", "id", "account")))
            .unwrap();
        assert!(
            matches!(credential, Credential::ChatGpt { refresh_token, account_id, .. }
            if refresh_token == "refresh" && account_id == "account")
        );
    }
}
