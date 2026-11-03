//! Real binary, SQLite, and HTTP transport without terminal or external credentials.
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Output};
use std::time::Duration;

fn run(directory: &std::path::Path, endpoint: &str, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_solaris"));
    command
        .env("SOLARIS_CONFIG_DIR", directory)
        .env("SOLARIS_LOCAL_BASE_URL", endpoint)
        .args(["--max-seconds", "5"])
        .args(args);
    for name in [
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "GOOGLE_API_KEY",
        "GEMINI_API_KEY",
        "OPENROUTER_API_KEY",
    ] {
        command.env_remove(name);
    }
    command.output().expect("run binary")
}

#[test]
fn json_run_can_be_resumed_and_exported_without_a_terminal() {
    let directory = tempfile::tempdir().expect("directory");
    std::fs::write(directory.path().join("models.json"), r#"{"models":[{"provider":"local","id":"test-model","capabilities":{"image_input":"supported"}}]}"#).expect("model metadata");
    let image_path = directory.path().join("pixel.png");
    std::fs::write(&image_path, b"\x89PNG\r\n\x1a\npayload").expect("image fixture");
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    listener.set_nonblocking(true).expect("nonblocking");
    let endpoint = format!("http://{}/v1", listener.local_addr().expect("address"));
    let server = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for _ in 0..2 {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && std::time::Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(10))
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            // Accepted sockets may inherit nonblocking mode on macOS; only
            // the listener needs it for the bounded accept loop above.
            socket.set_nonblocking(false).expect("blocking connection");
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .expect("timeout");
            let mut bytes = Vec::new();
            let body = loop {
                let mut block = [0; 4096];
                let count = socket.read(&mut block).expect("request");
                assert!(count > 0);
                bytes.extend_from_slice(&block[..count]);
                if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..end]);
                    let length: usize = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|value| value.trim().parse().expect("length"))
                        })
                        .expect("content length");
                    if bytes.len() >= end + 4 + length {
                        break serde_json::from_slice::<serde_json::Value>(
                            &bytes[end + 4..end + 4 + length],
                        )
                        .expect("json request");
                    }
                }
            };
            requests.push(body);
            let data = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"headless smoke ok\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{data}", data.len()).expect("response");
        }
        requests
    });
    let first = run(
        directory.path(),
        &endpoint,
        &[
            "--provider",
            "local",
            "--model",
            "test-model",
            "--print",
            "first prompt",
            "--json",
            "--image",
            image_path.to_str().expect("image path"),
        ],
    );
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let events: Vec<serde_json::Value> = String::from_utf8(first.stdout)
        .expect("utf8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("json event"))
        .collect();
    assert!(events.iter().all(|event| event["version"] == 1));
    let id = events[0]["session_id"].as_str().expect("session id");
    let store =
        solaris_agent::SessionStore::open(&directory.path().join("sessions/sessions.sqlite"))
            .expect("session store");
    let cwd = std::env::current_dir()
        .expect("cwd")
        .canonicalize()
        .expect("canonical cwd");
    let child = store
        .create_subagent(&cwd, "test-model", id)
        .expect("child");
    child
        .append(&solaris_agent::SessionRecord::Message {
            message: solaris_agent::Message::user("isolated child history"),
        })
        .expect("child history");
    assert_eq!(store.list(&cwd).expect("sessions")[0].id, child.id());
    drop(child);
    drop(store);
    let second = run(
        directory.path(),
        &endpoint,
        &["--continue", "--print", "second prompt"],
    );
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(String::from_utf8_lossy(&second.stdout).contains("headless smoke ok"));
    let exported = run(directory.path(), &endpoint, &["--export-session", id]);
    assert!(exported.status.success());
    let records: serde_json::Value =
        serde_json::from_slice(&exported.stdout).expect("exported journal");
    assert!(records.as_array().expect("records").len() >= 6);
    let requests = server.join().expect("server");
    assert!(
        requests[0]["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .any(|message| message["content"]
                .as_array()
                .is_some_and(|parts| parts.iter().any(|part| part["type"] == "image_url")))
    );
    assert!(
        requests[1]["messages"]
            .as_array()
            .expect("history")
            .iter()
            .any(|message| message["content"] == "first prompt")
    );
    assert!(
        !requests[1]["messages"]
            .as_array()
            .expect("history")
            .iter()
            .any(|message| message["content"] == "isolated child history")
    );
}

#[test]
fn automatic_continuation_keeps_user_forks_and_explicit_child_resume() {
    use solaris_agent::{SessionRecord, SessionStore};
    let directory = tempfile::tempdir().expect("directory");
    let store =
        SessionStore::open(&directory.path().join("sessions/sessions.sqlite")).expect("store");
    let cwd = std::env::current_dir()
        .expect("cwd")
        .canonicalize()
        .expect("canonical cwd");
    let root = store.create(&cwd, "test-model", None).expect("root");
    root.append(&SessionRecord::Model {
        provider: Some("local".into()),
        model: "test-model".into(),
    })
    .expect("model");
    let fork = root.fork(&cwd, "test-model").expect("user fork");
    let fork_id = fork.id().to_owned();
    let child = store
        .create_subagent(&cwd, "test-model", root.id())
        .expect("child");
    child
        .append(&SessionRecord::Model {
            provider: Some("local".into()),
            model: "test-model".into(),
        })
        .expect("model");
    let child_id = child.id().to_owned();
    let fork_records = store.records(&fork_id).expect("fork records").len();
    let child_records = store.records(&child_id).expect("child records").len();
    drop(root);
    drop(fork);
    drop(child);

    let output = run(
        directory.path(),
        "http://127.0.0.1:1/v1",
        &["--continue", "--rpc"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        store.records(&fork_id).expect("fork records").len(),
        fork_records + 1
    );
    assert_eq!(
        store.records(&child_id).expect("child records").len(),
        child_records
    );

    let output = run(
        directory.path(),
        "http://127.0.0.1:1/v1",
        &["--resume", &child_id, "--rpc"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        store.records(&child_id).expect("child records").len(),
        child_records + 1
    );
}

#[test]
fn automatic_continuation_refuses_a_directory_with_only_delegated_sessions() {
    let directory = tempfile::tempdir().expect("directory");
    let store =
        solaris_agent::SessionStore::open(&directory.path().join("sessions/sessions.sqlite"))
            .expect("store");
    let cwd = std::env::current_dir()
        .expect("cwd")
        .canonicalize()
        .expect("canonical cwd");
    let parent = store
        .create(
            &directory.path().join("another-project"),
            "test-model",
            None,
        )
        .expect("parent");
    let child = store
        .create_subagent(&cwd, "test-model", parent.id())
        .expect("child");
    drop(parent);
    drop(child);
    let output = run(
        directory.path(),
        "http://127.0.0.1:1/v1",
        &["--continue", "--rpc"],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no saved user session"));
}

#[test]
fn rpc_binary_accepts_commands_without_initializing_a_terminal() {
    use std::process::Stdio;
    let directory = tempfile::tempdir().expect("directory");
    let mut child = Command::new(env!("CARGO_BIN_EXE_solaris"))
        .env("SOLARIS_CONFIG_DIR", directory.path())
        .args(["--rpc", "--ephemeral", "--provider", "local"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("RPC binary");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"not json\n{\"id\":\"cancel\",\"command\":{\"type\":\"cancel\"}}\n")
        .expect("requests");
    let output = child.wait_with_output().expect("EOF exits");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let responses: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .expect("UTF8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSON response"))
        .collect();
    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0]["ok"], false);
    assert_eq!(responses[1]["id"], "cancel");
    assert_eq!(responses[1]["ok"], true);
}
