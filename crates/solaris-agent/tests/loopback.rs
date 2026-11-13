//! End-to-end test: a model asks for a tool over a real socket, the tool runs,
//! and the answer goes back for a second call.
//!
//! The unit tests drive the loop with a scripted backend and hold the tools in
//! hand. This one puts the real pieces together — a real `TcpListener`, a real
//! HTTP client, a real tool, the real agent — so that the request bodies, the
//! parsed tool call, the executed tool and the follow-up request are all
//! exercised together. Nothing here reaches the network.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use solaris_agent::{
    Agent, AgentEvent, AgentOptions, ConnectionConfig, Mode, Tool, ToolRegistry, ToolSelection,
};
use solaris_backend::Wire;
use solaris_backend::{Backend, Endpoint, TurnPlan};
use solaris_tools::tools::read::ReadTool;
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
    let delta = serde_json::json!({"choices":[{"delta":{"content":text}}]}).to_string();
    sse(&[
        &delta,
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
    ])
}

/// A backend pointed at `addr` speaking Chat Completions.
fn seed_memory(journal: &solaris_agent::SessionJournal, messages: Vec<solaris_agent::Message>) {
    let directory = journal
        .store()
        .artifact_dir()
        .parent()
        .unwrap()
        .join("memory")
        .join(journal.id());
    let store = rig_memory::DirStore::open(directory).unwrap();
    let mut native = rig_memory::load(&store, "conversation").unwrap();
    native.extend(
        messages
            .iter()
            .map(rig_core::message::Message::try_from)
            .collect::<Result<Vec<_>, _>>()
            .unwrap(),
    );
    rig_memory::save(&store, "conversation", &mut native).unwrap();
}

fn backend(addr: SocketAddr) -> Backend {
    backend_with_model(addr, solaris_agent::testing::parameters("test-model", 4096))
}

fn backend_with_model(addr: SocketAddr, model: solaris_backend::ModelParameters) -> Backend {
    Backend::new(TurnPlan {
        endpoint: Endpoint {
            wire: Wire::OpenAiChat,
            session_header: None,
            base_url: format!("http://{addr}/v1"),
            secret: Some("sk-loopback".to_string()),
            name: "Local runtime".into(),
            label: "local".into(),
            env_keys: &[],
        },
        model,

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

/// The agent over `addr`, offering nothing but the read tool.
///
/// A gateway answers no catalogue, so the tool is named outright rather than
/// left to a mode's default set.
fn agent(addr: SocketAddr, dir: &Path) -> Agent {
    let choice = ConnectionConfig {
        backend: Arc::new(backend(addr)),
        provider_id: None,
        model: "test-model".to_string(),
        fallback_context_window: 128_000,
        instructions: None,
    };
    let selection = ToolSelection {
        only: vec!["read".to_string()],
        ..Default::default()
    };
    Agent::new(
        choice,
        registry(),
        AgentOptions::in_directory(dir)
            .with_selection(selection)
            .with_approver(Arc::new(solaris_agent::AlwaysApprove)),
    )
}

/// Read a turn's stream to its end.
async fn drain(agent: &Agent, prompt: &str) -> Vec<AgentEvent> {
    let mut stream = agent.run_turn(prompt, Mode::Build);
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    events
}

/// A directory that lasts for one test.
fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("solaris-agent-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

#[tokio::test]
async fn a_tool_call_round_trips_through_a_real_socket() {
    let dir = temp_dir("loopback");
    std::fs::write(dir.join("a.txt"), "hello from the file\n").expect("fixture");

    let (addr, requests) = serve(vec![tool_call_body(), text_body("it says hello")]).await;
    let agent = agent(addr, &dir);

    let events = drain(&agent, "read a.txt").await;

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
    let agent = agent(addr, &dir);

    let events = drain(&agent, "hello").await;

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

#[tokio::test]
async fn native_connection_preserves_full_response_in_the_complete_executor() {
    use solaris_backend::ModelConnector;
    let (addr, seen) = serve(vec![sse(&[
        r#"{"id":"native-response","choices":[{"index":0,"delta":{"content":"native result"},"finish_reason":null}]}"#,
        r#"{"id":"native-response","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":12,"completion_tokens":3}}"#,
    ])]).await;
    let backend = Backend::new(TurnPlan {
        endpoint: Endpoint {
            wire: Wire::OpenAiChat,
            session_header: None,
            base_url: format!("http://{addr}/v1"),
            secret: Some("test".into()),
            name: "Test".into(),
            label: "local".into(),
            env_keys: &[],
        },
        model: solaris_agent::testing::parameters("native-model", 1234),

        prompt_cache_key: None,
    })
    .unwrap();
    let handlers = rig_agent::handlers::Handlers::default();
    handlers
        .register(
            "model",
            std::sync::Arc::new(backend).model_client().await.unwrap(),
        )
        .unwrap();
    let (run, events) = rig_agent::AgentBuilder::over_handlers(handlers, "native", "model".into())
        .build()
        .prompt("hello")
        .run_channel();
    drop(events);
    let result = run.await.unwrap();
    assert_eq!(result.output(), "native result");
    assert_eq!(result.usage.input_tokens, Some(12));
    assert_eq!(result.usage.output_tokens, Some(3));
    assert_eq!(
        result.completion_calls()[0].finish_reason,
        Some(rig_core::completion::FinishReason::Stop)
    );
    assert!(!result.completion_calls()[0].raw.is_null());
    let requests = tokio::time::timeout(PATIENCE, seen).await.unwrap().unwrap();
    let body: serde_json::Value =
        serde_json::from_str(requests[0].split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(body["model"], "native-model");
    assert_eq!(body["max_tokens"], 1234);
}

#[tokio::test]
async fn official_memory_is_the_only_message_store_and_failed_tasks_do_not_append() {
    let directory = tempfile::tempdir().unwrap();
    let denied = "{\"error\":{\"message\":\"request denied\"}}";
    let error = format!(
        "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        denied.len(),
        denied
    );
    let (addr, seen) = serve(vec![text_body("remembered"), error]).await;
    let store =
        solaris_agent::SessionStore::open(&directory.path().join("sessions.sqlite")).unwrap();
    let journal = store.create(directory.path(), "test-model", None).unwrap();
    let agent = agent(addr, directory.path());
    agent.attach_journal(Arc::clone(&journal)).unwrap();
    let events = drain(&agent, "remember this").await;
    assert!(
        matches!(events.last(), Some(AgentEvent::TurnComplete { .. })),
        "{events:?}"
    );
    let saved = journal.context().unwrap();
    assert_eq!(saved.len(), 2);
    assert_eq!(saved[1].text(), "remembered");
    let records = store.records(journal.id()).unwrap();
    assert!(
        !records
            .iter()
            .any(|record| matches!(record, solaris_agent::SessionRecord::Context { .. }))
    );
    assert!(
        directory
            .path()
            .join("memory")
            .join(journal.id())
            .join("logs/conversation.jsonl")
            .is_file()
    );
    let events = drain(&agent, "unsaved failed task").await;
    assert!(matches!(events.last(), Some(AgentEvent::Error(_))));
    assert_eq!(
        journal.context().unwrap(),
        saved,
        "SDK memory saves successful tasks only"
    );
    let records = store.records(journal.id()).unwrap();
    let execution: Vec<_> = records
        .iter()
        .filter_map(|record| match record {
            solaris_agent::SessionRecord::Execution { task, detail } => Some((task, detail)),
            _ => None,
        })
        .collect();
    assert!(
        execution
            .iter()
            .any(|(_, detail)| detail["event"] == "begin")
    );
    assert!(
        execution
            .iter()
            .any(|(_, detail)| detail["event"] == "complete")
    );
    assert!(
        execution
            .iter()
            .any(|(_, detail)| detail["event"] == "error")
    );
    assert_eq!(
        execution
            .iter()
            .map(|(task, _)| task.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        2
    );
    assert!(
        !serde_json::to_string(&execution)
            .unwrap()
            .contains("sk-loopback")
    );

    tokio::time::timeout(PATIENCE, seen).await.unwrap().unwrap();
}

#[tokio::test]
async fn manual_native_compaction_obeys_the_deadline_and_releases_the_connection() {
    let directory = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        let mut byte = [0];
        tokio::time::timeout(PATIENCE, socket.read(&mut byte))
            .await
            .unwrap()
            .unwrap()
    });
    let store =
        solaris_agent::SessionStore::open(&directory.path().join("sessions.sqlite")).unwrap();
    let journal = store.create(directory.path(), "test-model", None).unwrap();
    let saved = vec![
        solaris_backend::Message::user("earlier request"),
        solaris_backend::Message::assistant("earlier answer"),
        solaris_backend::Message::user("new request"),
    ];
    seed_memory(&journal, saved.clone());
    let mut options = AgentOptions::in_directory(directory.path());
    options.execution.budget.max_seconds = 1;
    options.compaction.keep_recent_tokens = 1;
    let agent = Agent::new(
        ConnectionConfig {
            backend: Arc::new(backend(addr)),
            model: "test-model".into(),
            provider_id: None,
            fallback_context_window: 128_000,
            instructions: None,
        },
        registry(),
        options,
    );
    agent.attach_journal(Arc::clone(&journal)).unwrap();
    assert_eq!(
        tokio::time::timeout(PATIENCE, agent.compact())
            .await
            .unwrap(),
        None
    );
    assert_eq!(journal.context().unwrap(), saved);
    assert_eq!(
        tokio::time::timeout(PATIENCE, server)
            .await
            .unwrap()
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn native_compaction_uses_sdk_limits_and_persists_the_sdk_summary_state() {
    let directory = tempfile::tempdir().unwrap();
    let (addr, seen) = serve(vec![text_body("keep the parser constraints")]).await;
    let store =
        solaris_agent::SessionStore::open(&directory.path().join("sessions.sqlite")).unwrap();
    let journal = store.create(directory.path(), "test-model", None).unwrap();
    seed_memory(
        &journal,
        vec![
            solaris_backend::Message::user("old request"),
            solaris_backend::Message::assistant("old answer"),
            solaris_backend::Message::user("new request"),
        ],
    );
    let spec = rig_core::catalog::ModelSpec::new(
        rig_core::providers::registry::ProviderId::declare("test").unwrap(),
        "test-model",
    )
    .with_tools(true)
    .with_context_window(8192)
    .with_max_output_tokens(4096);
    let model = backend_with_model(
        addr,
        solaris_backend::ModelParameters::new(spec, Some(4096), true),
    );
    let mut options = AgentOptions::in_directory(directory.path());
    options.compaction.keep_recent_tokens = 1;
    let agent = Agent::new(
        ConnectionConfig {
            backend: Arc::new(model),
            model: "test-model".into(),
            provider_id: None,
            fallback_context_window: 8192,
            instructions: None,
        },
        registry(),
        options,
    );
    agent.attach_journal(Arc::clone(&journal)).unwrap();
    assert_eq!(agent.compact().await, Some(2));
    let memory =
        rig_memory::DirStore::open(directory.path().join("memory").join(journal.id())).unwrap();
    let conversation = rig_memory::load(&memory, "conversation").unwrap();
    assert_eq!(
        rig_memory::summary_state(&conversation).summary,
        "keep the parser constraints"
    );
    assert_eq!(conversation.messages().len(), 2);
    assert!(rig_core::conversation::pairing::validate(conversation.messages()).is_ok());
    let requests = tokio::time::timeout(PATIENCE, seen).await.unwrap().unwrap();
    let body: serde_json::Value =
        serde_json::from_str(requests[0].split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(body["max_tokens"], 2048);
    assert!(body.get("tools").is_none());
}
