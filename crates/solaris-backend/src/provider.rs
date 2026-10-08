//! A backend that talks to a real provider over HTTP.

use std::collections::VecDeque;
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use futures::stream::BoxStream;
use reqwest::{Response, StatusCode};
use serde_json::Value;

use solaris_core::{
    AgentEvent, BackendError, Credential, Price, TurnRequest, Usage, Wire, max_output_for,
    price_for,
};

use crate::http;
use crate::protocols::anthropic;
use crate::protocols::openai::{self, Repair};
use crate::sse::{SseDecoder, SseEvent};
use crate::wire::{self, WireStream};
use crate::{AgentBackend, AgentEventStream};

/// Everything one request needs: where it goes, how it authenticates, and which
/// protocol it speaks.
#[derive(Debug, Clone)]
pub struct Endpoint {
    /// Provider id, shown in the status bar.
    pub provider_id: &'static str,
    /// Display name, used in error messages.
    pub provider_name: &'static str,
    /// Protocol to speak.
    pub wire: Wire,
    /// Base URL, without a trailing slash.
    pub base_url: String,
    /// The key or token to send, or `None` for a runtime that wants none.
    pub secret: Option<String>,
    /// Environment variables that could carry a key, for the error message.
    pub env_keys: &'static [&'static str],
}

impl Endpoint {
    /// The URL one turn is posted to.
    pub fn url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        match self.wire {
            Wire::AnthropicMessages => format!("{base}/v1/messages"),
            Wire::OpenAiChat => format!("{base}/chat/completions"),
            Wire::OpenAiResponses => format!("{base}/responses"),
        }
    }
}

/// Cache-routing key sent to OpenAI, which caches prompt prefixes without being
/// asked. The key groups a client's requests onto one cache shard, and a single
/// user wants every turn of a session on the same one.
const PROMPT_CACHE_KEY: &str = "solaris";

/// Whether `base_url` is OpenAI's own endpoint.
///
/// The cache key is only sent there. OpenAI's cache is automatic, so the field
/// buys routing and nothing else, while a compatible gateway that does not
/// implement it may reject the whole request — not worth the risk.
fn is_openai_endpoint(base_url: &str) -> bool {
    reqwest::Url::parse(base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .is_some_and(|host| host == "api.openai.com")
}

/// A backend that streams one turn from a real provider.
pub struct ProviderBackend {
    client: reqwest::Client,
    endpoint: Endpoint,
    model: String,
    max_tokens: u32,
    price: Option<Price>,
    prompt_cache_key: Option<&'static str>,
}

impl ProviderBackend {
    /// A backend that posts to `endpoint` using `model`.
    pub fn new(endpoint: Endpoint, model: &str) -> Result<Self, BackendError> {
        let prompt_cache_key = is_openai_endpoint(&endpoint.base_url).then_some(PROMPT_CACHE_KEY);

        Ok(Self {
            client: http::client()?,
            endpoint,
            model: model.to_string(),
            max_tokens: max_output_for(model),
            price: price_for(model),
            prompt_cache_key,
        })
    }

    /// Where requests go, for `--print-config`.
    pub fn base_url(&self) -> &str {
        &self.endpoint.base_url
    }

    /// The model this backend asks for.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Post one request, authenticating the way `wire` wants.
    async fn send(&self, url: &str, body: &Value) -> Result<Response, reqwest::Error> {
        let mut request = self.client.post(url).json(body);

        match self.endpoint.wire {
            Wire::AnthropicMessages => {
                request = request
                    .header("anthropic-version", anthropic::API_VERSION)
                    .header("accept", "text/event-stream");
                if let Some(secret) = &self.endpoint.secret {
                    request = request.header(anthropic::API_KEY_HEADER, secret);
                }
            }
            Wire::OpenAiChat | Wire::OpenAiResponses => {
                request = request.header("accept", "text/event-stream");
                if let Some(secret) = &self.endpoint.secret {
                    request = request.bearer_auth(secret);
                }
            }
        }

        request.send().await
    }

    /// A status line for an attempt that is about to be retried.
    fn retrying(&self, wait: Duration) -> AgentEvent {
        AgentEvent::Status(format!(
            "{} is busy — retrying in {:.1}s",
            self.endpoint.provider_name,
            wait.as_secs_f32()
        ))
    }

    /// A transport failure in the user's terms.
    fn unreachable(&self, error: &reqwest::Error) -> BackendError {
        BackendError::new(format!(
            "could not reach {}: {}",
            self.endpoint.provider_name,
            http::describe_transport(error)
        ))
    }
}

#[async_trait::async_trait]
impl AgentBackend for ProviderBackend {
    async fn run_turn(&self, request: TurnRequest) -> Result<AgentEventStream, BackendError> {
        let url = self.endpoint.url();
        let mut body = wire::request_body(
            self.endpoint.wire,
            &request,
            &self.model,
            self.max_tokens,
            self.prompt_cache_key,
        );
        let mut repaired: Vec<Repair> = Vec::new();
        let mut pending: VecDeque<AgentEvent> = VecDeque::new();
        let mut attempt = 0usize;

        let response = loop {
            match self.send(&url, &body).await {
                Ok(response) if response.status().is_success() => break response,
                Ok(response) => {
                    let status = response.status();
                    let retry_after = response.headers().clone();
                    let text = describe_body(response).await;

                    // A compatible server that names the field it dislikes is
                    // cheaper to obey than to guess about up front.
                    if status == StatusCode::BAD_REQUEST && self.endpoint.wire == Wire::OpenAiChat {
                        if let Some(repair) = openai::repair_for(&text) {
                            if !repaired.contains(&repair) {
                                openai::apply(&mut body, repair);
                                repaired.push(repair);
                                continue;
                            }
                        }
                    }

                    if http::is_retryable(status) && attempt < http::max_retries() {
                        let wait = http::backoff(attempt, Some(&retry_after));
                        // Nothing has been streamed yet, so a retry cannot
                        // duplicate anything the user has already read.
                        pending.push_back(self.retrying(wait));
                        attempt += 1;
                        tokio::time::sleep(wait).await;
                        continue;
                    }

                    return Err(BackendError::new(http::describe(
                        status,
                        &text,
                        self.endpoint.provider_name,
                        self.endpoint.env_keys,
                    )));
                }
                Err(error) if http::is_transient(&error) && attempt < http::max_retries() => {
                    let wait = http::backoff(attempt, None);
                    pending.push_back(self.retrying(wait));
                    attempt += 1;
                    tokio::time::sleep(wait).await;
                }
                Err(error) => return Err(self.unreachable(&error)),
            }
        };

        let engine = StreamEngine {
            stream: Box::pin(response.bytes_stream()),
            decoder: SseDecoder::new(),
            parser: WireStream::new(self.endpoint.wire),
            pending,
            input_characters: request_characters(&request),
            output_characters: 0,
            price: self.price,
            done: false,
        };

        Ok(Box::pin(futures::stream::unfold(
            engine,
            |mut engine| async move { engine.next_event().await.map(|event| (event, engine)) },
        )))
    }

    fn label(&self) -> &str {
        self.endpoint.provider_id
    }
}

/// Read a failed response's body for the provider's own explanation.
async fn describe_body(response: Response) -> String {
    match response.bytes().await {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(_) => String::new(),
    }
}

/// Characters of prompt and history, for the estimate fallback.
fn request_characters(request: &TurnRequest) -> usize {
    request.prompt.chars().count()
        + request
            .history
            .iter()
            .map(|message| message.content.chars().count())
            .sum::<usize>()
}

/// Drives one response body: bytes in, [`AgentEvent`]s out.
struct StreamEngine {
    stream: BoxStream<'static, Result<Bytes, reqwest::Error>>,
    decoder: SseDecoder,
    parser: WireStream,
    pending: VecDeque<AgentEvent>,
    input_characters: usize,
    output_characters: usize,
    price: Option<Price>,
    done: bool,
}

impl StreamEngine {
    /// The next event, pulling chunks until one is ready or the turn ends.
    async fn next_event(&mut self) -> Option<AgentEvent> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Some(event);
            }
            if self.done {
                return None;
            }

            match tokio::time::timeout(http::IDLE_TIMEOUT, self.stream.next()).await {
                Ok(Some(Ok(chunk))) => self.ingest(&chunk),
                Ok(Some(Err(error))) => {
                    self.fail(format!(
                        "the response was cut short: {}",
                        http::root_cause(&error)
                    ));
                }
                Ok(None) => {
                    let mut tail = Vec::new();
                    self.decoder.finish(&mut tail);
                    for event in &tail {
                        self.parse(event);
                    }
                    self.finish();
                }
                Err(_) => self.fail(format!(
                    "the provider stopped sending for {}s",
                    http::IDLE_TIMEOUT.as_secs()
                )),
            }
        }
    }

    /// Decode one chunk of body bytes.
    fn ingest(&mut self, chunk: &[u8]) {
        let mut events = Vec::new();
        self.decoder.push(chunk, &mut events);

        for event in &events {
            if self.done {
                return;
            }
            self.parse(event);
        }

        // A server that announced the end of the stream does not necessarily
        // close the connection straight away.
        if self.parser.finished() {
            self.finish();
        }
    }

    /// Hand one decoded frame to the wire parser.
    fn parse(&mut self, event: &SseEvent) {
        let mut produced = Vec::new();
        self.parser.handle(event, &mut produced);
        for event in produced {
            self.push(event);
        }
    }

    /// Queue an event, counting reply text for the estimate fallback.
    fn push(&mut self, event: AgentEvent) {
        if let AgentEvent::TextDelta(text) = &event {
            self.output_characters += text.chars().count();
        }
        self.pending.push_back(event);
    }

    /// Queue a terminal error and stop reading.
    fn fail(&mut self, message: String) {
        self.done = true;
        self.pending.push_back(AgentEvent::Error(message));
    }

    /// Emit the terminal event, exactly once.
    fn finish(&mut self) {
        if self.done {
            return;
        }
        self.done = true;

        // A provider that reports nothing at all still gets a token count, so
        // the footer gauge says something rather than nothing.
        let usage = self
            .parser
            .usage()
            .unwrap_or_else(|| Usage::estimate(self.input_characters, self.output_characters));
        let cost_usd = self.price.map(|price| price.cost(&usage)).unwrap_or(0.0);

        self.pending
            .push_back(AgentEvent::TurnComplete { usage, cost_usd });
    }
}

/// The secret a stored credential carries, or `None` when it carries none.
///
/// Kept here so the selection in [`crate::choose_backend`] stays about choosing
/// rather than about credentials.
pub fn secret_of(credential: &Credential) -> Option<String> {
    credential
        .secret()
        .map(|secret| secret.trim().to_string())
        .filter(|secret| !secret.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use solaris_core::{Message, Mode};

    #[test]
    fn only_openais_own_host_gets_the_cache_key() {
        assert!(is_openai_endpoint("https://api.openai.com/v1"));
        assert!(!is_openai_endpoint("https://openrouter.ai/api/v1"));
        assert!(!is_openai_endpoint(
            "https://generativelanguage.googleapis.com/v1beta/openai"
        ));
        assert!(!is_openai_endpoint("http://localhost:11434/v1"));
        // A lookalike host must not qualify — hence the host comparison rather
        // than a substring search.
        assert!(!is_openai_endpoint(
            "https://api.openai.com.example.test/v1"
        ));
        assert!(!is_openai_endpoint("not a url"));
    }

    #[test]
    fn a_backend_decides_the_cache_key_from_its_endpoint() {
        let build = |base_url: &str| {
            ProviderBackend::new(
                Endpoint {
                    provider_id: "openai",
                    provider_name: "OpenAI",
                    wire: Wire::OpenAiResponses,
                    base_url: base_url.to_string(),
                    secret: None,
                    env_keys: &[],
                },
                "gpt-5",
            )
            .expect("client")
        };

        let keyed = build("https://api.openai.com/v1");
        assert_eq!(keyed.prompt_cache_key, Some(PROMPT_CACHE_KEY));

        let unkeyed = build("https://gateway.internal/v1");
        assert_eq!(unkeyed.prompt_cache_key, None);
    }

    #[test]
    fn each_wire_has_its_own_path() {
        let endpoint = |wire| Endpoint {
            provider_id: "test",
            provider_name: "Test",
            wire,
            base_url: "https://example.test/".to_string(),
            secret: None,
            env_keys: &[],
        };

        assert_eq!(
            endpoint(Wire::AnthropicMessages).url(),
            "https://example.test/v1/messages"
        );
        assert_eq!(
            endpoint(Wire::OpenAiChat).url(),
            "https://example.test/chat/completions"
        );
        assert_eq!(
            endpoint(Wire::OpenAiResponses).url(),
            "https://example.test/responses"
        );
    }

    fn engine(chunks: Vec<Result<Bytes, reqwest::Error>>, wire: Wire) -> StreamEngine {
        StreamEngine {
            stream: Box::pin(futures::stream::iter(chunks)),
            decoder: SseDecoder::new(),
            parser: WireStream::new(wire),
            pending: VecDeque::new(),
            input_characters: 40,
            output_characters: 0,
            price: Some(Price::per_million(3.0, 15.0, 0.0, 0.0)),
            done: false,
        }
    }

    async fn drain(engine: &mut StreamEngine) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        while let Some(event) = engine.next_event().await {
            events.push(event);
        }
        events
    }

    #[tokio::test]
    async fn a_reported_turn_completes_with_the_reported_usage() {
        let mut engine = engine(
            vec![Ok(Bytes::from(
                "event: message_start\ndata: {\"message\":{\"usage\":{\"input_tokens\":1000,\"output_tokens\":1}}}\n\n\
                 event: content_block_delta\ndata: {\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n\
                 event: message_delta\ndata: {\"usage\":{\"output_tokens\":500}}\n\n\
                 event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            ))],
            Wire::AnthropicMessages,
        );

        let events = drain(&mut engine).await;
        assert_eq!(events[0], AgentEvent::TextDelta("hi".to_string()));
        assert_eq!(
            events.last(),
            Some(&AgentEvent::TurnComplete {
                usage: Usage::new(1000, 500),
                // 1000 input at $3/M plus 500 output at $15/M.
                cost_usd: 0.003 + 0.0075,
            })
        );
    }

    #[tokio::test]
    async fn a_turn_that_reports_nothing_is_estimated() {
        let mut engine = engine(vec![Ok(Bytes::from("data: [DONE]\n\n"))], Wire::OpenAiChat);

        let events = drain(&mut engine).await;
        let Some(AgentEvent::TurnComplete { usage, cost_usd }) = events.last() else {
            panic!("no terminal event: {events:?}");
        };
        assert!(usage.estimated, "an unreported turn must be flagged");
        assert_eq!(usage.input_tokens, 10, "40 characters → 10 tokens");
        assert!(*cost_usd > 0.0);
    }

    #[tokio::test]
    async fn a_stream_that_just_ends_still_completes_the_turn() {
        // No `message_stop`, no `[DONE]`: the connection simply closes.
        let mut engine = engine(
            vec![Ok(Bytes::from(
                "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            ))],
            Wire::OpenAiChat,
        );

        let events = drain(&mut engine).await;
        assert_eq!(events[0], AgentEvent::TextDelta("hi".to_string()));
        assert!(matches!(
            events.last(),
            Some(AgentEvent::TurnComplete { .. })
        ));
    }

    #[tokio::test]
    async fn a_frame_split_across_chunks_is_still_read() {
        let mut engine = engine(
            vec![
                Ok(Bytes::from(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"one",
                )),
                Ok(Bytes::from(" two\"}}]}\n\ndata: [DONE]\n\n")),
            ],
            Wire::OpenAiChat,
        );

        let events = drain(&mut engine).await;
        assert_eq!(events[0], AgentEvent::TextDelta("one two".to_string()));
        assert!(matches!(
            events.last(),
            Some(AgentEvent::TurnComplete { .. })
        ));
    }

    #[tokio::test]
    async fn a_broken_connection_ends_in_an_error_not_a_silent_completion() {
        let mut engine = engine(vec![Ok(Bytes::from("data: [DONE]\n\n"))], Wire::OpenAiChat);
        engine.fail("the response was cut short".to_string());

        let events = drain(&mut engine).await;
        assert!(
            matches!(events.first(), Some(AgentEvent::Error(_))),
            "{events:?}"
        );
        // An error is terminal on its own: no completion follows it.
        assert!(events.iter().all(|event| event.is_terminal()));
    }

    #[tokio::test]
    async fn retry_notes_are_delivered_before_the_reply() {
        let mut engine = engine(vec![Ok(Bytes::from("data: [DONE]\n\n"))], Wire::OpenAiChat);
        engine
            .pending
            .push_front(AgentEvent::Status("busy".to_string()));

        let events = drain(&mut engine).await;
        assert_eq!(events[0], AgentEvent::Status("busy".to_string()));
    }

    #[tokio::test]
    async fn an_unknown_model_costs_nothing_and_says_so() {
        let mut engine = engine(vec![Ok(Bytes::from("data: [DONE]\n\n"))], Wire::OpenAiChat);
        engine.price = None;

        let events = drain(&mut engine).await;
        let Some(AgentEvent::TurnComplete { cost_usd, .. }) = events.last() else {
            panic!("no terminal event");
        };
        assert_eq!(*cost_usd, 0.0);
    }

    #[test]
    fn only_a_stored_secret_is_used() {
        assert_eq!(
            secret_of(&Credential::ApiKey {
                key: " sk-1 ".to_string()
            }),
            Some("sk-1".to_string())
        );
        assert_eq!(
            secret_of(&Credential::Endpoint {
                base_url: "http://localhost:8080/v1".to_string(),
                api_key: "  ".to_string(),
            }),
            None,
            "an endpoint with no key sends no auth header"
        );
        assert_eq!(
            secret_of(&Credential::Token {
                token: "oauth".to_string()
            }),
            Some("oauth".to_string())
        );
    }

    #[test]
    fn the_request_length_is_counted_for_estimation() {
        let request = TurnRequest {
            history: vec![Message::system("12345"), Message::user("1234567890")],
            prompt: "123".to_string(),
            mode: Mode::Build,
        };
        assert_eq!(request_characters(&request), 18);
    }

    #[test]
    fn the_backend_is_labelled_with_its_provider_id() {
        let backend = ProviderBackend::new(
            Endpoint {
                provider_id: "openrouter",
                provider_name: "OpenRouter",
                wire: Wire::OpenAiChat,
                base_url: "https://openrouter.ai/api/v1".to_string(),
                secret: None,
                env_keys: &["OPENROUTER_API_KEY"],
            },
            "auto",
        )
        .expect("client");

        assert_eq!(backend.label(), "openrouter");
        assert_eq!(backend.model(), "auto");
        assert_eq!(backend.max_tokens, 8_192);
        // The routed model is billed at whichever upstream it picked, so there
        // is no list price to report.
        assert!(backend.price.is_none());
    }
}
