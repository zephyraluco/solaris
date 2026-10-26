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
        .args([
            "--provider",
            "local",
            "--model",
            "test-model",
            "--max-seconds",
            "5",
        ])
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
}
