//! Loopback tests: the whole client over a real socket, and nothing else.
//!
//! The unit tests drive the parsers from recorded frames and the streaming
//! engine from synthetic byte streams. These go one level down and one level up
//! at once — a real `TcpListener` on the loopback interface, a real `reqwest`
//! request, a real response body — so the plumbing between them (headers,
//! content length, chunked reads, teardown) is exercised too. Nothing here
//! reaches the network.

use std::net::SocketAddr;
use std::time::Duration;

use futures::StreamExt;
use solaris_backend::{AgentBackend, AgentEvent, Message, Price, TurnRequest, Usage, Wire};
use solaris_backend::{Backend, Endpoint, TurnPlan};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

/// How long a test waits for the fake server before giving up.
const PATIENCE: Duration = Duration::from_secs(10);

/// Start a one-shot HTTP server that captures one request and answers with
/// `response`, then closes the connection.
///
/// Returns the address to point a backend at, and a channel that yields the raw
/// request once the server has read it.
async fn serve(response: String) -> (SocketAddr, oneshot::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("address");
    let (tx, rx) = oneshot::channel();

    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let request = read_request(&mut socket).await;
        let _ = tx.send(request);

        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.flush().await;
        // Dropping the socket closes the connection, which is how the client
        // learns the body ended.
    });

    (addr, rx)
}

/// Read a whole request: the head, then `Content-Length` bytes of body.
async fn read_request(socket: &mut TcpStream) -> String {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];

    loop {
        let read = socket.read(&mut chunk).await.expect("read");
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);

        if let Some(end) = find(&buffer, b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buffer[..end]).into_owned();
            if buffer.len() >= end + 4 + content_length(&head).unwrap_or(0) {
                break;
            }
        }
    }

    String::from_utf8_lossy(&buffer).into_owned()
}

/// Index of the first occurrence of `needle`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// The `Content-Length` of a request head, if it declared one.
fn content_length(head: &str) -> Option<usize> {
    head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse().ok())?
    })
}

/// A response with a body of known length.
fn response(status: &str, content_type: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// A backend pointing at `addr`, named the way the local runtime is.
fn backend(addr: SocketAddr) -> Backend {
    backend_with(addr, Wire::OpenAiChat, "llama3.2", 32_768, None)
}

/// The same, for any wire and model, with the caller supplying what it would
/// have got from its own catalogue.
fn backend_with(
    addr: SocketAddr,
    wire: Wire,
    model: &str,
    max_tokens: u32,
    price: Option<Price>,
) -> Backend {
    Backend::new(TurnPlan {
        endpoint: Endpoint {
            wire,
            session_header: None,
            base_url: format!("http://{addr}/v1"),
            secret: Some("sk-loopback".to_string()),
            name: "Local runtime",
            label: "local",
            env_keys: &[],
        },
        model: model.to_string(),
        max_tokens,
        price,
        prompt_cache_key: None,
    })
    .expect("client")
}

fn turn() -> TurnRequest {
    TurnRequest {
        history: vec![Message::system("You are solaris."), Message::user("hi")],
        prompt: "hello".to_string(),
        tools: Vec::new(),
    }
}

#[tokio::test]
async fn chatgpt_profile_sends_account_header_and_supported_responses_fields() {
    let (addr, seen) = serve(response("200 OK", "text/event-stream",
        "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n")).await;
    let backend = backend_with(addr, Wire::OpenAiResponses, "gpt-5-codex", 8192, None)
        .with_chatgpt("account-123".into(), u64::MAX);
    let events: Vec<_> = backend.run_turn(turn()).await.unwrap().collect().await;
    assert!(events.iter().any(AgentEvent::is_terminal));
    let request = tokio::time::timeout(PATIENCE, seen).await.unwrap().unwrap();
    assert!(
        request
            .to_ascii_lowercase()
            .contains("chatgpt-account-id: account-123")
    );
    let (_, body) = request.split_once("\r\n\r\n").unwrap();
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(body["instructions"], "You are solaris.");
    assert_eq!(body["store"], false);
    assert!(body.get("max_output_tokens").is_none());
}

#[tokio::test]
async fn expired_chatgpt_credentials_are_refused_before_network_access() {
    let backend = backend_with(
        "127.0.0.1:9".parse().unwrap(),
        Wire::OpenAiResponses,
        "gpt-5",
        8192,
        None,
    )
    .with_chatgpt("account".into(), 0);
    let error = match backend.run_turn(turn()).await {
        Ok(_) => panic!("expired credentials must not reach the network"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("authorization expired"));
}

#[tokio::test]
async fn chatgpt_model_discovery_reads_codex_slugs_instead_of_public_api_ids() {
    let (addr, seen) = serve(response(
        "200 OK",
        "application/json",
        r#"{"models":[{"slug":"gpt-5-codex","visibility":"list","priority":2,"supported_in_api":false},{"slug":"hidden","visibility":"hide","priority":0},{"slug":"gpt-5","visibility":"list","priority":1}]}"#,
    ))
    .await;
    let backend = backend_with(addr, Wire::OpenAiResponses, "gpt-5-codex", 8192, None)
        .with_chatgpt("account".into(), u64::MAX);
    assert_eq!(
        backend.models().await.unwrap(),
        vec!["gpt-5", "gpt-5-codex"]
    );
    let request = seen.await.unwrap();
    assert!(request.starts_with("GET /v1/models?client_version=0.162.1 "));
    assert!(
        request
            .to_ascii_lowercase()
            .contains("chatgpt-account-id: account")
    );
}

#[tokio::test]
async fn a_streamed_reply_arrives_over_a_real_socket() {
    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n\
                data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n\
                data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                data: {\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":2}}\n\n\
                data: [DONE]\n\n";
    let (addr, seen) = serve(response("200 OK", "text/event-stream", body)).await;

    let events: Vec<AgentEvent> = backend(addr)
        .run_turn(turn())
        .await
        .expect("the turn started")
        .collect()
        .await;

    assert_eq!(events[0], AgentEvent::TextDelta("Hel".to_string()));
    assert_eq!(events[1], AgentEvent::TextDelta("lo".to_string()));

    let Some(AgentEvent::TurnComplete { usage, cost_usd }) = events.last() else {
        panic!("no terminal event: {events:?}");
    };
    assert_eq!(*usage, Usage::new(12, 2));
    assert_eq!(
        *cost_usd, 0.0,
        "a model running on this machine bills nothing"
    );

    // The request itself: where it went, who it claimed to be, and what it asked.
    let request = tokio::time::timeout(PATIENCE, seen)
        .await
        .expect("the server saw a request in time")
        .expect("the server reported its request")
        .to_lowercase();

    assert!(
        request.starts_with("post /v1/chat/completions "),
        "{request}"
    );
    assert!(
        request.contains("authorization: bearer sk-loopback"),
        "{request}"
    );
    assert!(
        request.contains("content-type: application/json"),
        "{request}"
    );
    assert!(request.contains("\"model\":\"llama3.2\""), "{request}");
    assert!(request.contains("\"stream\":true"), "{request}");
    assert!(request.contains("include_usage"), "{request}");
    assert!(request.contains("\"text\":\"hello\""), "{request}");
}

#[tokio::test]
async fn a_responses_turn_streams_over_a_real_socket() {
    // The Responses wire is the newest one, so it gets the same treatment: a
    // real request, a real streamed body, and the usage its closing event
    // carries. `gpt-5` is priced in the catalogue, so this also proves the
    // price lookup and the usage buckets line up.
    let body = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"content_index\":0,\"delta\":\"Hel\"}\n\n\
                event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"content_index\":0,\"delta\":\"lo\"}\n\n\
                event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":12,\"output_tokens\":2}}}\n\n";
    let (addr, seen) = serve(response("200 OK", "text/event-stream", body)).await;

    let events: Vec<AgentEvent> = backend_with(
        addr,
        Wire::OpenAiResponses,
        "gpt-5",
        128_000,
        // The rate the catalogue would have handed over.
        Some(Price::per_million(1.25, 10.00, 0.125, 0.00)),
    )
    .run_turn(turn())
    .await
    .expect("the turn started")
    .collect()
    .await;

    assert_eq!(events[0], AgentEvent::TextDelta("Hel".to_string()));
    assert_eq!(events[1], AgentEvent::TextDelta("lo".to_string()));

    let Some(AgentEvent::TurnComplete { usage, cost_usd }) = events.last() else {
        panic!("no terminal event: {events:?}");
    };
    assert_eq!(*usage, Usage::new(12, 2));
    // 12 input at $1.25/M plus 2 output at $10/M.
    assert!((*cost_usd - (12.0 * 1.25 + 2.0 * 10.0) / 1_000_000.0).abs() < 1e-12);

    let request = tokio::time::timeout(PATIENCE, seen)
        .await
        .expect("the server saw a request in time")
        .expect("the server reported its request")
        .to_lowercase();

    assert!(request.starts_with("post /v1/responses "), "{request}");
    assert!(
        request.contains("authorization: bearer sk-loopback"),
        "{request}"
    );
    assert!(
        request.contains("\"instructions\":\"you are solaris.\""),
        "{request}"
    );
    assert!(
        request.contains("\"max_output_tokens\":128000"),
        "{request}"
    );
    assert!(
        request.contains("\"store\":false"),
        "a terminal session must not store transcripts: {request}"
    );
    assert!(
        !request.contains("\"messages\""),
        "the Responses API takes input items, not messages: {request}"
    );
}

#[tokio::test]
async fn the_model_list_is_fetched_over_a_real_socket() {
    let body = r#"{"object":"list","data":[{"id":"glm-5.3"},{"id":"kimi-k3"}]}"#;
    let (addr, seen) = serve(response("200 OK", "application/json", body)).await;

    let models = backend(addr).models().await.expect("the list came back");
    assert_eq!(models, vec!["glm-5.3", "kimi-k3"]);

    let request = tokio::time::timeout(PATIENCE, seen)
        .await
        .expect("the server saw a request in time")
        .expect("the server reported its request")
        .to_lowercase();

    // A GET, to the list path, carrying the same credential a turn would.
    assert!(request.starts_with("get /v1/models "), "{request}");
    assert!(
        request.contains("authorization: bearer sk-loopback"),
        "{request}"
    );
}

#[tokio::test]
async fn a_refused_model_list_is_an_error_the_caller_falls_back_on() {
    let body = r#"{"error":{"message":"invalid api key provided"}}"#;
    let (addr, _seen) = serve(response("401 Unauthorized", "application/json", body)).await;

    let error = backend(addr)
        .models()
        .await
        .expect_err("a 401 must not read as a model list");

    let message = error.to_string();
    assert!(message.contains("Local runtime returned 401"), "{message}");
    assert!(message.contains("invalid api key provided"), "{message}");
}

#[tokio::test]
async fn a_web_page_where_the_model_list_should_be_is_reported_as_one() {
    // What a base URL that misses the gateway's API gets: its web front end,
    // which answers 200 with a page. Reading that as "no models" would hide a
    // wrong URL behind a plausible-looking answer.
    let (addr, _seen) = serve(response("200 OK", "text/html", "<html>hi</html>")).await;

    let error = backend(addr)
        .models()
        .await
        .expect_err("a web page is not a model list");

    let message = error.to_string();
    assert!(
        message.contains("with a web page, not a model list"),
        "{message}"
    );
    assert!(message.contains("/v1/models"), "{message}");
}

#[tokio::test]
async fn an_error_wrapped_in_a_200_is_reported_with_the_providers_words() {
    // New API answers 200 with `{"success": false, "message": …}` when it
    // cannot work out which models the token may use.
    let body = r#"{"success":false,"message":"get user group failed"}"#;
    let (addr, _seen) = serve(response("200 OK", "application/json", body)).await;

    let error = backend(addr)
        .models()
        .await
        .expect_err("an error body is not a model list");

    let message = error.to_string();
    assert!(
        message.contains("did not answer with a model list"),
        "{message}"
    );
    assert!(message.contains("get user group failed"), "{message}");
}

#[tokio::test]
async fn an_empty_model_list_is_an_answer_not_a_failure() {
    // The gateway answered: this credential simply reaches no models, which the
    // caller has to be able to tell apart from "that URL is not an API".
    let (addr, _seen) = serve(response(
        "200 OK",
        "application/json",
        r#"{"object":"list","data":[]}"#,
    ))
    .await;

    let models = backend(addr)
        .models()
        .await
        .expect("an empty list is a list");
    assert!(models.is_empty());
}

#[tokio::test]
async fn a_rejected_key_is_reported_with_the_providers_words() {
    let body = r#"{"error":{"message":"invalid api key provided"}}"#;
    let (addr, _seen) = serve(response("401 Unauthorized", "application/json", body)).await;

    // `AgentEventStream` is not `Debug`, so this cannot use `expect_err`.
    let error = match backend(addr).run_turn(turn()).await {
        Ok(_) => panic!("a 401 must not produce a stream"),
        Err(error) => error,
    };

    let message = error.to_string();
    assert!(message.contains("Local runtime returned 401"), "{message}");
    assert!(message.contains("invalid api key provided"), "{message}");
    assert!(
        message.contains("/connect"),
        "the message must say what to do: {message}"
    );
}

#[tokio::test]
async fn a_rate_limited_request_is_retried_before_it_streams() {
    // Answering 429 once and 200 the second time needs two connections, so this
    // listener serves a script instead of a single response.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("address");

    tokio::spawn(async move {
        let first = response(
            "429 Too Many Requests",
            "application/json",
            r#"{"error":{"message":"rate limited"}}"#,
        );

        let (mut socket, _) = listener.accept().await.expect("first accept");
        let _ = read_request(&mut socket).await;
        let _ = socket.write_all(first.as_bytes()).await;
        let _ = socket.flush().await;
        drop(socket);

        let second = response(
            "200 OK",
            "text/event-stream",
            "data: {\"choices\":[{\"delta\":{\"content\":\"at last\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
        );
        let (mut socket, _) = listener.accept().await.expect("second accept");
        let _ = read_request(&mut socket).await;
        let _ = socket.write_all(second.as_bytes()).await;
        let _ = socket.flush().await;
    });

    let events: Vec<AgentEvent> = backend(addr)
        .run_turn(turn())
        .await
        .expect("the retry succeeded")
        .collect()
        .await;

    // The retry is announced, because a silent pause would look like a hang.
    assert!(
        matches!(events.first(), Some(AgentEvent::Status(status)) if status.contains("retrying")),
        "{events:?}"
    );
    assert_eq!(events[1], AgentEvent::TextDelta("at last".to_string()));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnComplete { .. })
    ));
}

#[tokio::test]
async fn a_connection_that_answers_nothing_fails_before_it_streams() {
    // A server that accepts and closes without a byte: reqwest fails while
    // reading the status line. Nothing was streamed, so the failure comes back
    // from `run_turn` rather than as a stream that ends in silence.
    let (addr, _seen) = serve(String::new()).await;

    let error = match backend(addr).run_turn(turn()).await {
        Ok(_) => panic!("a closed connection is not a stream"),
        Err(error) => error,
    };

    let message = error.to_string();
    assert!(
        message.contains("could not reach Local runtime"),
        "{message}"
    );
}

fn completion(wire: Wire) -> String {
    match wire {
        Wire::OpenAiChat => "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2}}\n\ndata: [DONE]\n\n".into(),
        Wire::OpenAiResponses => "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"id\":\"msg\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"ok\",\"annotations\":[]}]}],\"usage\":{\"input_tokens\":10,\"output_tokens\":2}}}\n\n".into(),
        Wire::AnthropicMessages => concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"vision\",\"content\":[],\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        ).into(),
    }
}

#[tokio::test]
async fn user_and_tool_images_reach_all_three_rig_protocols_without_journal_metadata() {
    use solaris_backend::{Content, InlineImage, MessageContext, ToolCall, ToolResult};
    for wire in [
        Wire::OpenAiChat,
        Wire::OpenAiResponses,
        Wire::AnthropicMessages,
    ] {
        let image = InlineImage {
            media_type: "image/png".into(),
            data: "aW1hZ2U=".into(),
        };
        for tool_image in [false, true] {
            let mut history = if tool_image {
                let call = ToolCall::new(
                    "image-call",
                    "read",
                    serde_json::json!({"path":"image.png"}),
                );
                let mut result = ToolResult::ok(&call, "image result");
                result.metadata.images.push(image.clone());
                vec![Message::tool_use(call), Message::tool_result(result)]
            } else {
                vec![Message::with_content(
                    solaris_backend::Role::User,
                    vec![
                        Content::text("inspect"),
                        Content::Image {
                            image: image.clone(),
                        },
                    ],
                )]
            };
            history[0].context = Some(MessageContext::Measurement {
                usage: Usage::new(99999, 1),
                base_id: "private-base".into(),
                request_id: "private-request".into(),
            });
            let (addr, seen) =
                serve(response("200 OK", "text/event-stream", &completion(wire))).await;
            let mut input = TurnRequest::new(history, "");
            if tool_image {
                input.tools.push(solaris_backend::ToolSpec::new(
                    "read",
                    "Read an image",
                    serde_json::json!({"type":"object","properties":{"path":{"type":"string"}}}),
                ));
            }
            let events: Vec<_> = backend_with(addr, wire, "vision", 1000, None)
                .run_turn(input)
                .await
                .expect("start")
                .collect()
                .await;
            assert!(
                matches!(events.last(), Some(AgentEvent::TurnComplete { usage, .. }) if usage.total() == 12),
                "{wire:?}: {events:?}"
            );
            let request = seen.await.expect("request");
            let body = request.split_once("\r\n\r\n").expect("body").1;
            assert!(body.contains("aW1hZ2U="), "{wire:?}: {body}");
            assert!(
                !body.contains("private-base")
                    && !body.contains("private-request")
                    && !body.contains("99999")
            );
            if tool_image {
                assert!(
                    body.contains("image-call") && body.contains("image result"),
                    "{body}"
                );
            }
        }
    }
}

#[tokio::test]
async fn dropping_a_model_stream_closes_the_pending_socket() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        let frame = "data: {\"choices\":[{\"delta\":{\"content\":\"first\"}}]}\n\n";
        socket.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 999999\r\n\r\n{frame}").as_bytes()).await.unwrap();
        let mut byte = [0];
        socket.read(&mut byte).await.unwrap()
    });
    let mut stream = backend(addr).run_turn(turn()).await.unwrap();
    assert!(matches!(
        stream.next().await,
        Some(AgentEvent::TextDelta(_))
    ));
    drop(stream);
    assert_eq!(
        tokio::time::timeout(PATIENCE, server)
            .await
            .unwrap()
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn a_truncated_stream_is_not_retried_after_visible_output_or_reported_as_success() {
    let (addr, _) = serve("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 999999\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n".into()).await;
    let events: Vec<_> = backend(addr)
        .run_turn(turn())
        .await
        .unwrap()
        .collect()
        .await;
    assert!(matches!(events.first(), Some(AgentEvent::TextDelta(text)) if text == "partial"));
    assert!(matches!(events.last(), Some(AgentEvent::Error(_))));
    assert!(!events.iter().any(|event| matches!(
        event,
        AgentEvent::Status(_) | AgentEvent::TurnComplete { .. }
    )));
}

#[tokio::test]
async fn rejected_stream_options_are_removed_once_without_changing_prompt() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut bodies = Vec::new();
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await;
            let (_, body) = request.split_once("\r\n\r\n").unwrap();
            bodies.push(serde_json::from_str::<serde_json::Value>(body).unwrap());
            socket
                .write_all(
                    response(
                        "400 Bad Request",
                        "application/json",
                        r#"{"error":{"message":"unsupported stream_options"}}"#,
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        }
        bodies
    });
    let error = match tokio::time::timeout(PATIENCE, backend(addr).run_turn(turn()))
        .await
        .unwrap()
    {
        Err(error) => error,
        Ok(_) => panic!("repeated rejection must stop"),
    };
    assert!(error.to_string().contains("stream_options"));
    let bodies = tokio::time::timeout(PATIENCE, server)
        .await
        .unwrap()
        .unwrap();
    assert!(bodies[0].get("stream_options").is_some());
    assert!(bodies[1].get("stream_options").is_none());
    let mut expected = bodies[0].clone();
    expected.as_object_mut().unwrap().remove("stream_options");
    assert_eq!(expected, bodies[1]);
}

#[tokio::test]
async fn native_executor_connection_uses_the_same_retry_policy() {
    use rig_core::effect::{Answered, EffectId, EffectKind};
    use rig_core::serve::{Dispatch, Serve};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for reply in [
            response(
                "429 Too Many Requests",
                "application/json",
                r#"{"error":{"message":"rate limited"}}"#,
            ),
            response("200 OK", "text/event-stream", &completion(Wire::OpenAiChat)),
        ] {
            let (mut socket, _) = listener.accept().await.unwrap();
            requests.push(read_request(&mut socket).await);
            socket.write_all(reply.as_bytes()).await.unwrap();
        }
        requests
    });
    let client = backend(addr).model_client().await.unwrap().unwrap();
    let reply = client
        .serve(
            EffectKind::Completion {
                request: std::sync::Arc::new(rig_core::completion::CompletionRequest::new(
                    "retry me",
                )),
                stream: true,
            },
            Dispatch::new(EffectId::from_raw(1), true),
        )
        .await;
    let Answered::Completion(answer) = tokio::time::timeout(PATIENCE, reply.into_outcome())
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("completion expected")
    };
    assert_eq!(answer.text(), "ok");
    assert_eq!(answer.usage.input_tokens, Some(10));
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].split_once("\r\n\r\n").unwrap().1,
        requests[1].split_once("\r\n\r\n").unwrap().1
    );
}

#[tokio::test]
async fn official_messages_listing_follows_every_page_under_the_gateway_root() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for page in [
            r#"{"data":[{"id":"first","display_name":"First"}],"has_more":true,"last_id":"first"}"#,
            r#"{"data":[{"id":"second","display_name":"Second"}],"has_more":false,"last_id":"second"}"#,
        ] {
            let (mut socket, _) = listener.accept().await.unwrap();
            requests.push(read_request(&mut socket).await);
            socket
                .write_all(response("200 OK", "application/json", page).as_bytes())
                .await
                .unwrap();
        }
        requests
    });
    let backend = Backend::new(TurnPlan {
        endpoint: Endpoint {
            wire: Wire::AnthropicMessages,
            session_header: Some("x-opencode-session"),
            base_url: format!("http://{addr}/gateway/v1"),
            secret: Some("test-key".into()),
            name: "Gateway",
            label: "custom",
            env_keys: &[],
        },
        model: "test".into(),
        max_tokens: 100,
        price: None,
        prompt_cache_key: None,
    })
    .unwrap();
    let ids = tokio::time::timeout(PATIENCE, backend.models())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ids, ["first", "second"]);
    let requests = server.await.unwrap();
    assert!(requests[0].starts_with("GET /gateway/v1/models HTTP/1.1"));
    assert!(requests[1].starts_with("GET /gateway/v1/models?after_id=first HTTP/1.1"));
    for request in requests {
        assert!(request.contains("x-api-key: test-key"));
        assert!(request.contains("x-opencode-session:"));
    }
}
