//! HTTP plumbing shared by both wire protocols: one client, one error
//! vocabulary, one retry policy.

use std::time::Duration;

use reqwest::header::{HeaderMap, RETRY_AFTER};
use reqwest::{Client, StatusCode};

use solaris_core::BackendError;

/// How long a connect may take before it counts as a failure.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a streaming response may sit idle between two chunks. Generous,
/// because a model can think for a while before its first token.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// Backoff between attempts; the length is also the number of retries.
const BACKOFF: [Duration; 3] = [
    Duration::from_millis(400),
    Duration::from_millis(1_200),
    Duration::from_millis(3_600),
];

/// Longest pause a `Retry-After` header may impose on us.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);

/// Longest provider error message kept from a response body.
const MAX_DETAIL: usize = 200;

/// Build the client both wires share.
///
/// Deliberately no whole-request timeout: a turn legitimately streams for
/// minutes, so only the gap between two chunks is bounded.
pub fn client() -> Result<Client, BackendError> {
    Client::builder()
        .user_agent(concat!("solaris/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .map_err(|error| BackendError::new(format!("could not set up HTTP: {error}")))
}

/// How many times a failed attempt is worth repeating.
pub fn max_retries() -> usize {
    BACKOFF.len()
}

/// How long to wait before retry number `attempt` (zero based).
///
/// The server's own `Retry-After` wins when it sent one, but is never allowed
/// to hold the turn for longer than [`MAX_RETRY_AFTER`].
pub fn backoff(attempt: usize, headers: Option<&HeaderMap>) -> Duration {
    let asked = headers.and_then(retry_after).unwrap_or_default();
    let scheduled = BACKOFF[attempt.min(BACKOFF.len() - 1)];
    asked.clamp(scheduled, MAX_RETRY_AFTER)
}

/// Whether `status` is worth retrying.
///
/// Only ever consulted before the first event reaches the UI, where a retry
/// cannot duplicate text the user has already seen.
pub fn is_retryable(status: StatusCode) -> bool {
    status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::CONFLICT
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

/// Whether a transport error is worth retrying.
pub fn is_transient(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect()
}

/// The innermost message of a reqwest error.
///
/// The top-level display repeats the URL and the method; the root cause is the
/// one that names what actually went wrong — DNS, TLS, a refused connection.
pub fn root_cause(error: &reqwest::Error) -> String {
    let mut source: &dyn std::error::Error = error;
    while let Some(next) = source.source() {
        source = next;
    }
    source.to_string()
}

/// A transport failure in the user's terms.
pub fn describe_transport(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "the request timed out".to_string()
    } else if error.is_connect() {
        format!("could not connect ({})", root_cause(error))
    } else {
        root_cause(error)
    }
}

/// A message the user can act on, preferring the provider's own wording.
pub fn describe(status: StatusCode, body: &str, provider: &str, env_keys: &[&str]) -> String {
    let hint = if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        match env_keys.first() {
            Some(key) => format!(" — run /connect to replace the key, or check {key}"),
            None => " — run /connect to sign in again".to_string(),
        }
    } else if status == StatusCode::PAYMENT_REQUIRED {
        " — the account has no funds left, so top it up or check the plan in the provider's console"
            .to_string()
    } else if status == StatusCode::NOT_FOUND {
        " — the model or endpoint may be wrong, check /model".to_string()
    } else if status == StatusCode::TOO_MANY_REQUESTS {
        " — rate limited, try again shortly".to_string()
    } else {
        String::new()
    };

    let mut message = format!("{provider} returned {status}{hint}");
    if let Some(detail) = provider_message(body) {
        message.push_str(&format!(": {detail}"));
    }
    message
}

/// The message a provider wrapped its error in, when it sent one.
pub fn provider_message(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body.trim()).ok()?;

    let error = value.get("error");
    let message = error
        .and_then(|error| {
            error
                .get("message")
                .and_then(serde_json::Value::as_str)
                .or_else(|| error.as_str())
        })
        .or_else(|| value.get("message").and_then(serde_json::Value::as_str))
        .or_else(|| value.get("detail").and_then(serde_json::Value::as_str))?;

    Some(ellipsise(message))
}

/// Trim `text` to something that fits one status line.
fn ellipsise(text: &str) -> String {
    let single_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if single_line.chars().count() <= MAX_DETAIL {
        return single_line;
    }
    let head: String = single_line.chars().take(MAX_DETAIL - 1).collect();
    format!("{head}…")
}

/// Seconds the server asked us to wait.
fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?;
    value.trim().parse::<u64>().ok().map(Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(retry_after: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(value) = retry_after {
            headers.insert(RETRY_AFTER, value.parse().expect("header value"));
        }
        headers
    }

    #[test]
    fn retryable_statuses_are_the_transient_ones() {
        assert!(is_retryable(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable(StatusCode::BAD_GATEWAY));
        assert!(is_retryable(StatusCode::SERVICE_UNAVAILABLE));
        assert!(is_retryable(StatusCode::REQUEST_TIMEOUT));

        // A rejected key or a bad model is not going to fix itself.
        assert!(!is_retryable(StatusCode::UNAUTHORIZED));
        assert!(!is_retryable(StatusCode::BAD_REQUEST));
        assert!(!is_retryable(StatusCode::NOT_FOUND));
    }

    #[test]
    fn backoff_grows_and_honours_retry_after() {
        let plain = |attempt| backoff(attempt, None);
        assert!(plain(0) < plain(1));
        assert!(plain(1) < plain(2));
        // The last entry repeats rather than panicking.
        assert_eq!(plain(9), plain(2));

        // A server asking for longer than our schedule gets what it asked for.
        let asked = backoff(0, Some(&headers(Some("5"))));
        assert_eq!(asked, Duration::from_secs(5));

        // …but never longer than the cap.
        let capped = backoff(0, Some(&headers(Some("600"))));
        assert_eq!(capped, MAX_RETRY_AFTER);

        // A server asking for less than our schedule does not shorten it.
        assert_eq!(backoff(2, Some(&headers(Some("0")))), plain(2));

        // A malformed header is ignored rather than fatal.
        assert_eq!(backoff(0, Some(&headers(Some("soon")))), plain(0));
    }

    #[test]
    fn error_messages_prefer_the_providers_wording() {
        let openai =
            r#"{"error":{"message":"Incorrect API key provided","type":"invalid_request_error"}}"#;
        assert_eq!(
            provider_message(openai).as_deref(),
            Some("Incorrect API key provided")
        );

        let anthropic = r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
        assert_eq!(
            provider_message(anthropic).as_deref(),
            Some("invalid x-api-key")
        );

        // Some compatible servers use a bare string or a `detail` field.
        assert_eq!(
            provider_message(r#"{"error":"bad model"}"#).as_deref(),
            Some("bad model")
        );
        assert_eq!(
            provider_message(r#"{"detail":"not found"}"#).as_deref(),
            Some("not found")
        );

        assert_eq!(provider_message("<html>502</html>"), None);
        assert_eq!(provider_message(""), None);
    }

    #[test]
    fn long_provider_messages_are_flattened_and_trimmed() {
        let body = format!(r#"{{"error":{{"message":"{}"}}}}"#, "x".repeat(500));
        let message = provider_message(&body).expect("message");
        assert!(message.ends_with('…'));
        assert_eq!(message.chars().count(), MAX_DETAIL);

        let multiline = r#"{"error":{"message":"line one\n\n  line two"}}"#;
        assert_eq!(
            provider_message(multiline).as_deref(),
            Some("line one line two")
        );
    }

    #[test]
    fn a_rejected_key_says_how_to_fix_it() {
        let message = describe(
            StatusCode::UNAUTHORIZED,
            r#"{"error":{"message":"invalid x-api-key"}}"#,
            "Anthropic",
            &["ANTHROPIC_API_KEY"],
        );
        assert!(message.contains("Anthropic returned 401"), "{message}");
        assert!(message.contains("ANTHROPIC_API_KEY"), "{message}");
        assert!(message.contains("invalid x-api-key"), "{message}");

        // A provider with no environment variable gets the generic advice.
        let message = describe(StatusCode::UNAUTHORIZED, "", "Custom endpoint", &[]);
        assert!(message.contains("run /connect"), "{message}");
    }

    #[test]
    fn an_empty_account_says_where_to_top_it_up() {
        let message = describe(
            StatusCode::PAYMENT_REQUIRED,
            r#"{"error":{"message":"Insufficient account funds"}}"#,
            "OpenCode Go",
            &[],
        );
        assert!(message.contains("OpenCode Go returned 402"), "{message}");
        assert!(message.contains("console"), "{message}");
        assert!(message.contains("Insufficient account funds"), "{message}");
    }

    #[test]
    fn an_unexplained_status_still_reads_as_a_sentence() {
        let message = describe(StatusCode::BAD_REQUEST, "not json", "Groq", &[]);
        assert_eq!(message, "Groq returned 400 Bad Request");
    }
}
