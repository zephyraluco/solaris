use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::time::Duration;

pub fn message_contains(message: &serde_json::Value, expected: &str) -> bool {
    message["content"]
        .as_str()
        .is_some_and(|text| text.contains(expected))
        || message["content"].as_array().is_some_and(|parts| {
            parts.iter().any(|part| {
                part["text"]
                    .as_str()
                    .is_some_and(|text| text.contains(expected))
            })
        })
}

pub fn command(directory: &std::path::Path, endpoint: &str, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_solaris"));
    command
        .env("SOLARIS_CONFIG_DIR", directory)
        .env("SOLARIS_LOCAL_BASE_URL", endpoint)
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
    command
}

pub fn sse_reply(delta: serde_json::Value, finish: &str) -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
        serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":finish}]})
    )
}

pub fn serve_responses(
    responses: Vec<String>,
) -> (String, std::thread::JoinHandle<Vec<serde_json::Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    listener.set_nonblocking(true).expect("nonblocking");
    let endpoint = format!("http://{}/v1", listener.local_addr().expect("address"));
    let server = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for data in responses {
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
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{data}", data.len()).expect("response");
        }
        requests
    });
    (endpoint, server)
}
