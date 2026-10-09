//! The HTTP client every platform is reached through.

use std::collections::VecDeque;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures::StreamExt;
use futures::stream::BoxStream;
use reqwest::{Response, StatusCode};
use serde_json::Value;

use solaris_core::{AgentEvent, BackendError, Price, TurnRequest, Usage};

use crate::http;
use crate::protocols::anthropic;
use crate::protocols::openai::{self, Repair};
use crate::sse::{SseDecoder, SseEvent};
use crate::wire::{self, Wire, WireStream};
use crate::{AgentBackend, AgentEventStream};

/// Where one turn goes, how it authenticates, and which protocol to speak.
///
/// Fixed at the moment the backend is built: nothing here is looked up later.
#[derive(Debug, Clone)]
pub struct Endpoint {
    /// Protocol to speak.
    pub wire: Wire,
    /// Header the platform wants a conversation id in, when it asks for one.
    pub session_header: Option<&'static str>,
    /// Base URL, without a trailing slash.
    pub base_url: String,
    /// The key or token to send, or `None` for a runtime that wants none.
    pub secret: Option<String>,
    /// Display name, used in error messages.
    pub name: &'static str,
    /// Short label shown in the status bar — the platform id.
    pub label: &'static str,
    /// Environment variables that could carry a key, for the error message.
    pub env_keys: &'static [&'static str],
}

impl Endpoint {
    /// The URL one turn is posted to.
    ///
    /// The API version is part of the base URL, so a gateway that serves several
    /// protocols under one root — opencode go answers `/responses`, `/messages`
    /// and `/chat/completions` beneath `/zen/go/v1` — needs no more than one entry
    /// in this match per protocol it speaks.
    pub fn url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        match self.wire {
            Wire::AnthropicMessages => format!("{base}/messages"),
            Wire::OpenAiChat => format!("{base}/chat/completions"),
            Wire::OpenAiResponses => format!("{base}/responses"),
        }
    }

    /// The URL the model list is fetched from.
    ///
    /// Both protocols expose the same `{"data": [{"id": …}]}` shape. The
    /// Messages API pages its list at twenty by default, so the largest page it
    /// allows is asked for; a list longer than that still shows its first page.
    pub fn models_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        match self.wire {
            Wire::AnthropicMessages => format!("{base}/models?limit=1000"),
            Wire::OpenAiChat | Wire::OpenAiResponses => format!("{base}/models"),
        }
    }
}

/// An id for this run, stable for as long as the process lives.
///
/// One process is one conversation here, which is exactly the granularity a
/// platform routing by session asks for.
fn session_id() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        format!("solaris-{nanos:x}-{:x}", std::process::id())
    })
}

/// One turn, already resolved by the caller.
///
/// Everything platform-specific has been decided before this point: which
/// endpoint, which model, how large a reply to ask for, what that model costs,
/// and whether a prompt-cache key should ride along.
#[derive(Debug, Clone)]
pub struct TurnPlan {
    /// Where the turn goes and how it authenticates.
    pub endpoint: Endpoint,
    /// Model id to send, exactly as the caller wants it.
    pub model: String,
    /// Largest reply to ask for.
    pub max_tokens: u32,
    /// List price for the model, when the caller knows one. The backend only
    /// multiplies; it has no idea what any model costs by itself.
    pub price: Option<Price>,
    /// Prompt-cache routing key, for the platforms that want one.
    pub prompt_cache_key: Option<&'static str>,
}

/// Streams one turn from an HTTP endpoint, over whichever wire it speaks.
pub struct HttpBackend {
    client: reqwest::Client,
    endpoint: Endpoint,
    model: String,
    max_tokens: u32,
    price: Option<Price>,
    prompt_cache_key: Option<&'static str>,
}

impl HttpBackend {
    /// A backend for `plan`.
    pub fn new(plan: TurnPlan) -> Result<Self, BackendError> {
        Ok(Self {
            client: http::client()?,
            endpoint: plan.endpoint,
            model: plan.model,
            max_tokens: plan.max_tokens,
            price: plan.price,
            prompt_cache_key: plan.prompt_cache_key,
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

    /// Attach the credential and the headers the wire wants.
    ///
    /// A key travels the way the protocol carrying it does: `x-api-key` on the
    /// Messages API, a bearer token on the OpenAI ones — opencode's Go gateway
    /// included, whose `/v1/messages` takes `x-api-key` while its
    /// OpenAI-compatible paths take a bearer token.
    fn authorize(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let request = match self.endpoint.wire {
            Wire::AnthropicMessages => request.header("anthropic-version", anthropic::API_VERSION),
            Wire::OpenAiChat | Wire::OpenAiResponses => request,
        };

        let request = match &self.endpoint.secret {
            Some(secret) if self.endpoint.wire.is_openai() => request.bearer_auth(secret),
            Some(secret) => request.header(anthropic::API_KEY_HEADER, secret),
            None => request,
        };

        // A platform that routes by conversation is told which one this is, so
        // the turn lands on the plan it belongs to rather than on whichever
        // balance the gateway would otherwise guess at.
        match self.endpoint.session_header {
            Some(header) => request.header(header, session_id()),
            None => request,
        }
    }

    /// Post one request, authenticating the way `wire` wants.
    async fn send(&self, url: &str, body: &Value) -> Result<Response, reqwest::Error> {
        self.authorize(self.client.post(url).json(body))
            .header("accept", "text/event-stream")
            .send()
            .await
    }

    /// A status line for an attempt that is about to be retried.
    fn retrying(&self, wait: Duration) -> AgentEvent {
        AgentEvent::Status(format!(
            "{} is busy — retrying in {:.1}s",
            self.endpoint.name,
            wait.as_secs_f32()
        ))
    }

    /// A transport failure in the user's terms.
    fn unreachable(&self, error: &reqwest::Error) -> BackendError {
        BackendError::new(format!(
            "could not reach {}: {}",
            self.endpoint.name,
            http::describe_transport(error)
        ))
    }

    /// Why a successful response carried no model list.
    fn not_a_model_list(&self, url: &str, body: &str) -> String {
        let name = self.endpoint.name;
        if body.trim_start().starts_with('<') {
            return format!("{name} answered {url} with a web page, not a model list");
        }

        let reason = http::provider_message(body)
            .map(|message| format!(": {message}"))
            .unwrap_or_default();
        format!("{name} did not answer with a model list at {url}{reason}")
    }
}

#[async_trait::async_trait]
impl AgentBackend for HttpBackend {
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
                        self.endpoint.name,
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
        self.endpoint.label
    }

    /// Ask the provider which models it offers.
    ///
    /// One attempt, no retries: this feeds a picker, so a slow or unreachable
    /// provider should cost nothing more than falling back to the catalogue.
    async fn models(&self) -> Result<Vec<String>, BackendError> {
        let url = self.endpoint.models_url();
        let response = self
            .authorize(self.client.get(&url))
            .header("accept", "application/json")
            .send()
            .await
            .map_err(|error| self.unreachable(&error))?;

        let status = response.status();
        let body = describe_body(response).await;

        if !status.is_success() {
            return Err(BackendError::new(http::describe(
                status,
                &body,
                self.endpoint.name,
                self.endpoint.env_keys,
            )));
        }

        // A 200 that is not a model list is a failure, not an empty provider: a
        // base URL that stops short of a gateway's API gets its web front end
        // back, which answers 200 with a page, and reading that as "no models"
        // would hide a wrong URL behind a plausible-looking answer.
        let Some(models) = parse_models(&body) else {
            return Err(BackendError::new(self.not_a_model_list(&url, &body)));
        };

        Ok(models)
    }
}

/// Model ids out of a `{"data": [{"id": …}]}` list.
///
/// Both wires answer with this shape. `None` means the body was not a model
/// list at all — a proxy's HTML page, an error wrapped in a 200 — which the
/// caller reports instead of passing off as a provider with nothing to offer.
/// An empty `data` array is a list: the provider answered, and its answer is
/// that this credential can reach no models.
fn parse_models(body: &str) -> Option<Vec<String>> {
    let value: Value = serde_json::from_str(body).ok()?;
    let entries = value["data"].as_array()?;

    Some(
        entries
            .iter()
            .filter_map(|entry| entry["id"].as_str())
            .map(str::to_owned)
            .collect(),
    )
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
            .map(|message| message.characters())
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

        // A message the model stopped writing because it ran out of room may
        // carry tool calls with half-written arguments, so the readers are told
        // before the calls themselves go out.
        if self.parser.truncated() {
            self.pending.push_back(AgentEvent::OutputTruncated);
        }

        // A stream only knows a tool call is complete once it ends, so the
        // calls go out here — before the terminal event, so the turn reads as
        // text, then calls, then completion.
        let mut calls = Vec::new();
        self.parser.flush(&mut calls);
        for event in calls {
            self.push(event);
        }

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

#[cfg(test)]
mod tests {
    use super::*;
    use solaris_core::{Message, Mode};

    #[test]
    fn each_wire_has_its_own_path() {
        // The base carries the API version, so a wire appends only what is its
        // own.
        let endpoint = |wire| Endpoint {
            wire,
            session_header: None,
            base_url: "https://example.test/v1/".to_string(),
            secret: None,
            name: "Test",
            label: "Test",
            env_keys: &[],
        };

        assert_eq!(
            endpoint(Wire::AnthropicMessages).url(),
            "https://example.test/v1/messages"
        );
        assert_eq!(
            endpoint(Wire::OpenAiChat).url(),
            "https://example.test/v1/chat/completions"
        );
        assert_eq!(
            endpoint(Wire::OpenAiResponses).url(),
            "https://example.test/v1/responses"
        );

        // The Messages API pages its list, so the largest page is asked for.
        assert_eq!(
            endpoint(Wire::AnthropicMessages).models_url(),
            "https://example.test/v1/models?limit=1000"
        );
        assert_eq!(
            endpoint(Wire::OpenAiChat).models_url(),
            "https://example.test/v1/models"
        );
        assert_eq!(
            endpoint(Wire::OpenAiResponses).models_url(),
            "https://example.test/v1/models"
        );
    }

    #[test]
    fn one_gateway_root_serves_every_wire() {
        // opencode's Go plan answers all three protocols beneath one root,
        // `/zen/go/v1`, which is why the version lives in the base and each wire
        // appends only the path that is its own.
        let endpoint = |wire| Endpoint {
            wire,
            session_header: Some("x-opencode-session"),
            base_url: "https://opencode.ai/zen/go/v1".to_string(),
            secret: None,
            name: "opencode go",
            label: "opencode",
            env_keys: &[],
        };

        assert_eq!(
            endpoint(Wire::OpenAiChat).url(),
            "https://opencode.ai/zen/go/v1/chat/completions"
        );
        assert_eq!(
            endpoint(Wire::OpenAiResponses).url(),
            "https://opencode.ai/zen/go/v1/responses"
        );
        assert_eq!(
            endpoint(Wire::AnthropicMessages).url(),
            "https://opencode.ai/zen/go/v1/messages"
        );
        assert_eq!(
            endpoint(Wire::OpenAiChat).models_url(),
            "https://opencode.ai/zen/go/v1/models"
        );
    }

    #[test]
    fn a_platform_that_routes_by_conversation_is_told_which_one() {
        let endpoint = Endpoint {
            wire: Wire::OpenAiChat,
            session_header: Some("x-opencode-session"),
            base_url: "https://opencode.ai/zen/go/v1".to_string(),
            secret: Some("sk-go".to_string()),
            name: "opencode go",
            label: "opencode",
            env_keys: &[],
        };
        let backend = HttpBackend::new(TurnPlan {
            endpoint,
            model: "m".to_string(),
            max_tokens: 1,
            price: None,
            prompt_cache_key: None,
        })
        .expect("a client");

        let headers = backend
            .authorize(
                backend
                    .client
                    .post("https://opencode.ai/zen/go/v1/chat/completions"),
            )
            .build()
            .expect("a request")
            .headers()
            .clone();

        let session = headers
            .get("x-opencode-session")
            .expect("a session id")
            .to_str()
            .expect("an ascii id");
        assert!(session.starts_with("solaris-"), "{session}");
        // Stable for the run, so the gateway can group a conversation.
        assert_eq!(session, session_id());
        // And the key still goes where the gateway asked for it.
        assert_eq!(headers.get("authorization").expect("a key"), "Bearer sk-go");
    }

    #[test]
    fn a_key_travels_the_way_its_wire_asks_for_it() {
        let headers = |wire| {
            let backend = HttpBackend::new(TurnPlan {
                endpoint: Endpoint {
                    wire,
                    session_header: None,
                    base_url: "https://example.test/v1".to_string(),
                    secret: Some("sk-1".to_string()),
                    name: "Test",
                    label: "Test",
                    env_keys: &[],
                },
                model: "m".to_string(),
                max_tokens: 1,
                price: None,
                prompt_cache_key: None,
            })
            .expect("a client");

            backend
                .authorize(backend.client.post("https://example.test/x"))
                .build()
                .expect("a request")
                .headers()
                .clone()
        };

        // Anthropic wants its key in its own header, with its version beside it.
        let anthropic = headers(Wire::AnthropicMessages);
        assert_eq!(anthropic.get("x-api-key").expect("a key"), "sk-1");
        assert!(anthropic.get("authorization").is_none());
        assert!(anthropic.get("anthropic-version").is_some());

        // The OpenAI wires take a bearer token, and know no Anthropic version.
        for wire in [Wire::OpenAiChat, Wire::OpenAiResponses] {
            let sent = headers(wire);
            assert_eq!(sent.get("authorization").expect("a key"), "Bearer sk-1");
            assert!(sent.get("x-api-key").is_none());
            assert!(sent.get("anthropic-version").is_none());
        }
    }

    #[test]
    fn model_ids_are_read_from_both_list_shapes() {
        // An OpenAI-compatible server.
        assert_eq!(
            parse_models(r#"{"object":"list","data":[{"id":"glm-5.3"},{"id":"kimi-k3"}]}"#),
            Some(vec!["glm-5.3".to_string(), "kimi-k3".to_string()])
        );

        // The Messages API answers with the same `data[].id`, surrounded by
        // paging fields this does not care about.
        assert_eq!(
            parse_models(
                r#"{"data":[{"id":"claude-sonnet-4-5","display_name":"Sonnet"}],"has_more":false}"#
            ),
            Some(vec!["claude-sonnet-4-5".to_string()])
        );
    }

    #[test]
    fn anything_that_is_not_a_model_list_reads_as_none() {
        assert_eq!(parse_models(""), None);
        assert_eq!(parse_models("<html>a proxy said no</html>"), None);
        assert_eq!(
            parse_models(r#"{"error":{"message":"invalid api key"}}"#),
            None
        );
        assert_eq!(parse_models(r#"{"data":"not a list"}"#), None);
        // An entry without an id is skipped rather than invented.
        assert_eq!(
            parse_models(r#"{"data":[{"object":"model"},{"id":"ok"}]}"#),
            Some(vec!["ok".to_string()])
        );
        // An empty list is a list: the provider answered, with nothing.
        assert_eq!(
            parse_models(r#"{"object":"list","data":[]}"#),
            Some(Vec::new())
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
    fn the_request_length_is_counted_for_estimation() {
        let request = TurnRequest {
            history: vec![Message::system("12345"), Message::user("1234567890")],
            prompt: "123".to_string(),
            mode: Mode::Build,
            tools: Vec::new(),
        };
        assert_eq!(request_characters(&request), 18);
    }

    #[test]
    fn the_backend_repeats_what_it_was_handed() {
        let backend = HttpBackend::new(TurnPlan {
            endpoint: Endpoint {
                wire: Wire::OpenAiChat,
                session_header: None,
                base_url: "https://openrouter.ai/api/v1".to_string(),
                secret: None,
                name: "OpenRouter",
                label: "openrouter",
                env_keys: &["OPENROUTER_API_KEY"],
            },
            model: "auto".to_string(),
            max_tokens: 8_192,
            price: None,
            prompt_cache_key: None,
        })
        .expect("client");

        // A transport crate decides none of this; it only carries it.
        assert_eq!(backend.label(), "openrouter");
        assert_eq!(backend.model(), "auto");
        assert_eq!(backend.max_tokens, 8_192);
        assert!(backend.price.is_none());
    }
}
