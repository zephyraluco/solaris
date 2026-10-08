//! End-to-end test: a model asks for a tool over a real socket, the tool runs,
//! and the answer goes back for a second call.
//!
//! The unit tests drive the loop with a scripted backend and the tools from
//! canned outputs. This one puts the real pieces together — a real `TcpListener`,
//! a real HTTP client, the real tools — so that the request bodies, the parsed
//! tool call, the executed tool and the follow-up request are all exercised
//! together. Nothing here reaches the network.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use solaris_backend::{AgentBackend, Endpoint, HttpBackend, TurnPlan, Wire};
use solaris_core::{AgentEvent, Message, Mode, TurnRequest};
use solaris_tools::tools::read::ReadTool;
use solaris_tools::{Tool, ToolLoop, ToolLoopOptions, ToolRegistry};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

/// How long the test waits for the fake server before giving up.
const PATIENCE: Duration = Duration::from_secs(10);

/// Start a server that answers `responses` in order, one connection each, and
/// hands back every request it read.
async fn serve(responses: Vec<String>) -> (SocketAddr, oneshot::Receiver<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("address");
    let (tx, rx) = oneshot::channel();

    tokio::spawn(async move {
        let mut requests = Vec::new();
        for response in responses {
            let (mut socket, _) = listener.accept().await.expect("accept");
            requests.push(read_request(&mut socket).await);

            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
            // Dropping the socket closes the connection, which is how the
            // client learns the body ended.
        }
        let _ = tx.send(requests);
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

/// An event-stream response carrying `events`.
fn sse(events: &[&str]) -> String {
    let mut body = String::new();
    for event in events {
        body.push_str("data: ");
        body.push_str(event);
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// The first answer: the model asks to read a file.
fn tool_call_body() -> String {
    sse(&[
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","function":{"name":"read","arguments":"{\"path\":\"a.txt\"}"}}]}}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
    ])
}

/// The second answer: the model writes what it found.
fn text_body(text: &str) -> String {
    sse(&[&format!(
        r#"{{"choices":[{{"delta":{{"content":"{text}"}}}}]}}"#
    )])
}

/// A backend pointed at `addr` speaking Chat Completions.
fn backend(addr: SocketAddr) -> HttpBackend {
    HttpBackend::new(TurnPlan {
        endpoint: Endpoint {
            wire: Wire::OpenAiChat,
            session_header: None,
            base_url: format!("http://{addr}/v1"),
            secret: Some("sk-loopback".to_string()),
            name: "Local runtime",
            label: "local",
            env_keys: &[],
        },
        model: "test-model".to_string(),
        max_tokens: 4096,
        price: None,
        prompt_cache_key: None,
    })
    .expect("client")
}

/// A registry holding only the tool this test needs.
fn registry() -> Arc<ToolRegistry> {
    Arc::new(ToolRegistry::with_tools(vec![
        Arc::new(ReadTool::new()) as Arc<dyn Tool>
    ]))
}

/// A directory that lasts for one test.
fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("solaris-tools-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

#[tokio::test]
async fn a_tool_call_round_trips_through_a_real_socket() {
    let dir = temp_dir("loopback");
    std::fs::write(dir.join("a.txt"), "hello from the file\n").expect("fixture");

    let (addr, requests) = serve(vec![tool_call_body(), text_body("it says hello")]).await;
    let backend = ToolLoop::new(
        Arc::new(backend(addr)),
        registry(),
        ToolLoopOptions::in_directory(PathBuf::from(&dir)),
    );

    let request = TurnRequest::new(
        vec![Message::system("You are solaris.")],
        "read a.txt",
        Mode::Build,
    );
    let mut stream = backend.run_turn(request).await.expect("started");

    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }

    // The tool ran, and its output came back to the caller.
    let results: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolResult { result, .. } => Some(result.output.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 1);
    assert!(
        results[0].contains("hello from the file"),
        "the real read tool ran: {results:?}"
    );

    // The model's final text arrived, and exactly one terminal event did.
    let text: String = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::TextDelta(chunk) => Some(chunk.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "it says hello");
    assert_eq!(events.iter().filter(|event| event.is_terminal()).count(), 1);

    // Both requests went out: the first declared the tool, the second carried
    // the answer back.
    let requests = tokio::time::timeout(PATIENCE, requests)
        .await
        .expect("the server answered both requests")
        .expect("requests");
    assert_eq!(requests.len(), 2, "a tool round trip is two model calls");
    assert!(requests[0].contains(r#""tools""#), "{}", requests[0]);
    assert!(requests[0].contains(r#""name":"read""#), "{}", requests[0]);
    assert!(
        requests[1].contains(r#""role":"tool""#),
        "the result goes back as a tool message: {}",
        requests[1]
    );
    assert!(
        requests[1].contains("hello from the file"),
        "with the text the tool produced: {}",
        requests[1]
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_turn_with_no_tool_call_is_a_single_request() {
    let dir = temp_dir("plain");

    let (addr, requests) = serve(vec![text_body("no tools needed")]).await;
    let backend = ToolLoop::new(
        Arc::new(backend(addr)),
        registry(),
        ToolLoopOptions::in_directory(dir.clone()),
    );

    let mut stream = backend
        .run_turn(TurnRequest::new(Vec::new(), "hello", Mode::Build))
        .await
        .expect("started");
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }

    assert!(
        events.iter().all(|event| !matches!(
            event,
            AgentEvent::ToolCall(_) | AgentEvent::ToolResult { .. }
        )),
        "{events:?}"
    );
    let requests = tokio::time::timeout(PATIENCE, requests)
        .await
        .expect("the server answered")
        .expect("requests");
    assert_eq!(requests.len(), 1, "one call in, one call out");

    let _ = std::fs::remove_dir_all(&dir);
}
