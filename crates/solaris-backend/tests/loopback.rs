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
use solaris_backend::{AgentBackend, Endpoint, HttpBackend, TurnPlan, Wire};
use solaris_core::{AgentEvent, Message, Mode, Price, TurnRequest, Usage};
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
fn backend(addr: SocketAddr) -> HttpBackend {
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
) -> HttpBackend {
    HttpBackend::new(TurnPlan {
        endpoint: Endpoint {
            wire,
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
        mode: Mode::Build,
    }
}

#[tokio::test]
async fn a_streamed_reply_arrives_over_a_real_socket() {
    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n\
                data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n\
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
    assert!(request.contains("\"content\":\"hello\""), "{request}");
}

#[tokio::test]
async fn a_responses_turn_streams_over_a_real_socket() {
    // The Responses wire is the newest one, so it gets the same treatment: a
    // real request, a real streamed body, and the usage its closing event
    // carries. `gpt-5` is priced in the catalogue, so this also proves the
    // price lookup and the usage buckets line up.
    let body = "event: response.output_text.delta\ndata: {\"delta\":\"Hel\"}\n\n\
                event: response.output_text.delta\ndata: {\"delta\":\"lo\"}\n\n\
                event: response.completed\ndata: {\"response\":{\"usage\":{\"input_tokens\":12,\"output_tokens\":2}}}\n\n";
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
            "data: {\"choices\":[{\"delta\":{\"content\":\"at last\"}}]}\n\ndata: [DONE]\n\n",
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
