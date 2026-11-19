//! Real binary, SQLite, and HTTP transport without terminal or external credentials.
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Output};
use std::time::Duration;

fn message_contains(message: &serde_json::Value, expected: &str) -> bool {
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

fn command(directory: &std::path::Path, endpoint: &str, args: &[&str]) -> Command {
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

fn run(directory: &std::path::Path, endpoint: &str, args: &[&str]) -> Output {
    command(directory, endpoint, args)
        .output()
        .expect("run binary")
}

fn sse_reply(delta: serde_json::Value, finish: &str) -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
        serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":finish}]})
    )
}

fn serve_responses(
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

#[test]
fn json_run_can_be_resumed_and_exported_without_a_terminal() {
    let directory = tempfile::tempdir().expect("directory");
    std::fs::write(directory.path().join("models.json"), r#"{"models":{"test-model":{"provider":"local","request_id":"test-model","metadata":"ollama/test-model"}},"overrides":{"ollama":{"models":{"test-model":{"tool_call":true,"reasoning":false,"modalities":{"input":["text","image"],"output":["text"]}}}}}}"#).expect("model metadata");
    let image_path = directory.path().join("pixel.png");
    std::fs::write(&image_path, b"\x89PNG\r\n\x1a\npayload").expect("image fixture");
    let response = sse_reply(serde_json::json!({"content":"headless smoke ok"}), "stop");
    let (endpoint, server) = serve_responses(vec![response.clone(), response]);
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
    memory::seed(
        &child,
        vec![rig_core::message::Message::user("isolated child history")],
    );
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
            .any(|message| message_contains(message, "first prompt"))
    );
    assert!(
        !requests[1]["messages"]
            .as_array()
            .expect("history")
            .iter()
            .any(|message| message_contains(message, "isolated child history"))
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

#[test]
fn cli_fork_restores_the_source_provider_unless_explicitly_overridden() {
    use rig_core::message::Message;
    use solaris_agent::{SessionRecord, SessionStore};
    use solaris_provider::{AuthStore, Credential};
    let directory = tempfile::tempdir().expect("directory");
    let store =
        SessionStore::open(&directory.path().join("sessions/sessions.sqlite")).expect("store");
    let cwd = std::env::current_dir()
        .expect("cwd")
        .canonicalize()
        .expect("canonical cwd");
    let source = store.create(&cwd, "test-model", None).expect("source");
    source
        .append(&SessionRecord::Model {
            provider: Some("local".into()),
            model: "test-model".into(),
        })
        .expect("source model");
    memory::seed(&source, vec![Message::user("source private history")]);
    let source_id = source.id().to_owned();
    drop(source);
    let mut auth = AuthStore::new();
    auth.store(
        "openai",
        Credential::Endpoint {
            base_url: "http://127.0.0.1:1/v1".into(),
            api_key: "test-only".into(),
        },
    );
    auth.activate("openai");
    std::fs::write(
        directory.path().join("auth.json"),
        auth.to_json().expect("auth"),
    )
    .expect("save auth");
    for override_provider in [false, true] {
        let mut args = vec!["--fork", &source_id, "--rpc"];
        if override_provider {
            args.extend(["--provider", "openai"]);
        }
        let output = run(directory.path(), "http://127.0.0.1:1/v1", &args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let fork = store
            .list(&cwd)
            .expect("sessions")
            .into_iter()
            .find(|info| info.parent_id.as_deref() == Some(&source_id))
            .expect("fork");
        assert_eq!(
            fork.provider.as_deref(),
            Some(if override_provider { "openai" } else { "local" })
        );
        let journal = store.resume(&fork.id).expect("fork released");
        assert_eq!(
            memory::messages(&journal)[0].rag_text().as_deref(),
            Some("source private history")
        );
    }
    assert_eq!(
        store
            .list(&cwd)
            .expect("sessions")
            .into_iter()
            .find(|info| info.id == source_id)
            .expect("source")
            .provider
            .as_deref(),
        Some("local")
    );
}

#[test]
fn headless_confirmation_preserves_automatic_reads_and_refuses_required_approvals() {
    for (case, confirm, ask_tool, tool, allowed) in [
        ("confirm reads", true, None, "read", true),
        ("unrelated ask rule", false, Some("write"), "read", true),
        ("explicit read ask", true, Some("read"), "read", false),
        ("mutating call", true, None, "write", false),
    ] {
        let directory = tempfile::tempdir().expect("directory");
        let source = directory.path().join("source.txt");
        let target = directory.path().join("must-not-be-created.txt");
        std::fs::write(&source, "policy fixture content").expect("source file");
        if let Some(tool) = ask_tool {
            std::fs::write(
                directory.path().join("permissions.json"),
                serde_json::json!([
                    {"tool":tool,"resource":"*","effect":"ask"}
                ])
                .to_string(),
            )
            .expect("rules");
        }
        let input = if tool == "read" {
            serde_json::json!({"path":source})
        } else {
            serde_json::json!({"path":target,"content":"blocked"})
        };
        let call = sse_reply(
            serde_json::json!({"tool_calls":[{
                "index":0,"id":"policy-call","type":"function",
                "function":{"name":tool,"arguments":input.to_string()}
            }]}),
            "tool_calls",
        );
        let (endpoint, server) = serve_responses(vec![
            call,
            sse_reply(serde_json::json!({"content":"policy done"}), "stop"),
        ]);
        let mut args = vec![
            "--provider",
            "local",
            "--model",
            "test-model",
            "--print",
            "test permissions",
            "--json",
            "--ephemeral",
        ];
        if confirm {
            args.push("--confirm-tools");
        }
        if tool == "read" {
            args.push("--plan");
        }
        let output = run(directory.path(), &endpoint, &args);
        assert!(
            output.status.success(),
            "{case}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let requests = server.join().expect("fixture server");
        let events: Vec<serde_json::Value> = String::from_utf8(output.stdout)
            .expect("UTF8")
            .lines()
            .map(|line| serde_json::from_str(line).expect("event"))
            .collect();
        let result = &events
            .iter()
            .find_map(|event| event["event"].get("ToolResult"))
            .expect("tool result")["result"];
        assert_eq!(result["is_error"], !allowed, "{case}: {result}");
        if allowed {
            assert!(
                result["output"]
                    .as_str()
                    .expect("output")
                    .contains("policy fixture content")
            );
            assert!(
                requests[1]["messages"]
                    .as_array()
                    .expect("messages")
                    .iter()
                    .any(|message| message["role"] == "tool"
                        && message["content"]
                            .as_str()
                            .is_some_and(|text| text.contains("policy fixture content")))
            );
        }
        assert!(!target.exists(), "confirmation must not allow writes");
    }
}

#[test]
fn named_model_routes_to_custom_endpoint_with_original_request_alias() {
    let directory = tempfile::tempdir().expect("directory");
    let (endpoint, server) = serve_responses(vec![sse_reply(
        serde_json::json!({"content":"named route ok"}),
        "stop",
    )]);
    let config = serde_json::json!({
        "providers":{"gateway":{"protocol":"openai-chat","base_url":endpoint}},
        "models":{"fast":{"provider":"gateway","request_id":"private-alias","metadata":"anthropic/claude-sonnet-4-5","output_tokens":2048}}
    });
    std::fs::write(directory.path().join("models.json"), config.to_string()).expect("config");
    let output = run(
        directory.path(),
        "http://127.0.0.1:1/v1",
        &["--model", "fast", "--print", "hello", "--ephemeral"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("named route ok"));
    let requests = server.join().expect("server");
    assert_eq!(requests[0]["model"], "private-alias");
    let printed = run(
        directory.path(),
        "http://127.0.0.1:1/v1",
        &["--model", "fast", "--print-config"],
    );
    assert!(printed.status.success());
    let printed = String::from_utf8_lossy(&printed.stdout);
    assert!(printed.contains("backend: gateway"), "{printed}");
    assert!(printed.contains("anthropic/claude-sonnet-4-5"), "{printed}");
}

#[path = "../../solaris-agent/tests/support/memory.rs"]
mod memory;

struct AcpProcess {
    child: std::process::Child,
    incoming: std::sync::mpsc::Receiver<serde_json::Value>,
}

impl AcpProcess {
    fn start(config: &std::path::Path, endpoint: &str, extra: &[&str]) -> Self {
        Self::start_with_capabilities(config, endpoint, extra, serde_json::json!({}))
    }

    fn start_with_capabilities(
        config: &std::path::Path,
        endpoint: &str,
        extra: &[&str],
        capabilities: serde_json::Value,
    ) -> Self {
        use std::io::BufRead;
        use std::process::Stdio;
        let mut child = command(
            config,
            endpoint,
            &["--acp", "--provider", "local", "--model", "test-model"],
        )
        .args(extra)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("start ACP process");
        let stdout = child.stdout.take().unwrap();
        let (tx, incoming) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines() {
                let frame: serde_json::Value = serde_json::from_str(&line.unwrap()).unwrap();
                if tx.send(frame).is_err() {
                    break;
                }
            }
        });
        let mut peer = Self { child, incoming };
        peer.send(serde_json::json!({"id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":capabilities}}));
        let (frame, _) = peer.response(1);
        assert_eq!(frame["result"]["protocolVersion"], 1, "{frame}");
        assert_eq!(frame["result"]["agentCapabilities"]["loadSession"], true);
        assert_eq!(
            frame["result"]["agentCapabilities"]["promptCapabilities"]["image"],
            true
        );
        assert_eq!(
            frame["result"]["agentCapabilities"]["mcpCapabilities"]["http"],
            true
        );
        peer
    }

    fn send(&mut self, mut frame: serde_json::Value) {
        frame["jsonrpc"] = serde_json::json!("2.0");
        writeln!(self.child.stdin.as_mut().unwrap(), "{frame}").unwrap();
    }

    fn recv(&self) -> serde_json::Value {
        self.incoming
            .recv_timeout(Duration::from_secs(12))
            .expect("ACP frame")
    }

    fn response(&self, id: u64) -> (serde_json::Value, Vec<serde_json::Value>) {
        let mut updates = Vec::new();
        loop {
            let frame = self.recv();
            if frame["id"] == id && frame.get("method").is_none() {
                return (frame, updates);
            }
            updates.push(frame);
        }
    }

    fn new_session(&mut self, cwd: &std::path::Path) -> String {
        self.send(
            serde_json::json!({"id":2,"method":"session/new","params":{"cwd":cwd,"mcpServers":[]}}),
        );
        let (frame, _) = self.response(2);
        frame["result"]["sessionId"]
            .as_str()
            .unwrap_or_else(|| panic!("{frame}"))
            .to_owned()
    }

    fn close(mut self) {
        drop(self.child.stdin.take());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success());
                return;
            }
            assert!(std::time::Instant::now() < deadline, "ACP must exit on EOF");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for AcpProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn acp_streams_turns_and_replays_the_saved_session_on_reopen() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("workspace with spaces");
    std::fs::create_dir(&cwd).unwrap();
    let (endpoint, server) = serve_responses(vec![
        sse_reply(json!({"content":"first reply"}), "stop"),
        sse_reply(json!({"content":"second reply"}), "stop"),
    ]);
    let mut peer = AcpProcess::start(dir.path(), &endpoint, &[]);
    let id = peer.new_session(&cwd);
    peer.send(json!({"id":3,"method":"session/set_config_option","params":{"sessionId":id,"configId":"mode","value":"plan"}}));
    assert_eq!(
        peer.response(3).0["result"]["configOptions"][0]["currentValue"],
        "plan"
    );
    for (request_id, prompt, reply) in [
        (4, "first prompt", "first reply"),
        (5, "second prompt", "second reply"),
    ] {
        peer.send(json!({"id":request_id,"method":"session/prompt","params":{"sessionId":id,"prompt":[{"type":"text","text":prompt}]}}));
        let (stop, frames) = peer.response(request_id);
        assert_eq!(stop["result"]["stopReason"], "end_turn", "{stop}");
        assert!(
            frames
                .iter()
                .any(|frame| frame["params"]["update"]["content"]["text"] == reply)
        );
    }
    peer.close();
    let mut restored = AcpProcess::start(dir.path(), &endpoint, &[]);
    restored.send(
        json!({"id":2,"method":"session/load","params":{"sessionId":id,"cwd":cwd,"mcpServers":[]}}),
    );
    let (frame, updates) = restored.response(2);
    assert!(frame.get("result").is_some(), "{frame}");
    let text: Vec<_> = updates
        .iter()
        .filter_map(|frame| frame["params"]["update"]["content"]["text"].as_str())
        .collect();
    assert_eq!(
        text,
        [
            "first prompt",
            "first reply",
            "second prompt",
            "second reply"
        ]
    );
    restored.close();
    assert_eq!(server.join().unwrap().len(), 2);
}

#[test]
fn acp_cancels_a_turn_waiting_for_tool_approval() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let (endpoint, server) = serve_responses(vec![sse_reply(
        json!({"tool_calls":[{"index":0,"id":"write-1","type":"function","function":{"name":"write","arguments":"{\"path\":\"marker.txt\",\"content\":\"must not run\"}"}}]}),
        "tool_calls",
    )]);
    let mut peer = AcpProcess::start(dir.path(), &endpoint, &["--confirm-tools"]);
    let id = peer.new_session(dir.path());
    peer.send(json!({"id":3,"method":"session/prompt","params":{"sessionId":id,"prompt":[{"type":"text","text":"write a file"}]}}));
    let permission = loop {
        let frame = peer.recv();
        assert_ne!(frame["id"], 3, "{frame}");
        if frame["method"] == "session/request_permission" {
            break frame;
        }
    };
    assert_eq!(permission["params"]["sessionId"], id);
    peer.send(json!({"method":"session/cancel","params":{"sessionId":id}}));
    peer.send(json!({"id":permission["id"],"result":{"outcome":{"outcome":"cancelled"}}}));
    assert_eq!(peer.response(3).0["result"]["stopReason"], "cancelled");
    assert!(!dir.path().join("marker.txt").exists());
    peer.close();
    assert_eq!(server.join().unwrap().len(), 1);
}

#[test]
fn acp_tool_approval_controls_execution_and_updates_the_same_call() {
    use serde_json::json;
    for allowed in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (endpoint, server) = serve_responses(vec![
            sse_reply(
                json!({"tool_calls":[{"index":0,"id":"write-1","type":"function","function":{"name":"write","arguments":"{\"path\":\"marker.txt\",\"content\":\"approved\"}"}}]}),
                "tool_calls",
            ),
            sse_reply(json!({"content":"done"}), "stop"),
        ]);
        let mut peer = AcpProcess::start(dir.path(), &endpoint, &["--confirm-tools"]);
        let id = peer.new_session(dir.path());
        peer.send(json!({"id":3,"method":"session/prompt","params":{"sessionId":id,"prompt":[{"type":"text","text":"write a file"}]}}));
        let mut tool_id = None;
        let permission = loop {
            let frame = peer.recv();
            assert_ne!(frame["id"], 3, "{frame}");
            if frame["method"] == "session/request_permission" {
                break frame;
            }
            if frame["params"]["update"]["sessionUpdate"] == "tool_call" {
                tool_id = frame["params"]["update"]["toolCallId"]
                    .as_str()
                    .map(str::to_owned);
            }
        };
        let approval_id = &permission["params"]["toolCall"]["toolCallId"];
        peer.send(json!({"id":permission["id"],"result":{"outcome":{"outcome":"selected","optionId":if allowed {"allow"} else {"reject"}}}}));
        let (response, updates) = peer.response(3);
        assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
        for frame in &updates {
            let update = &frame["params"]["update"];
            if update["sessionUpdate"] == "tool_call" {
                tool_id = update["toolCallId"].as_str().map(str::to_owned);
            }
        }
        assert_eq!(tool_id.as_deref(), approval_id.as_str());
        assert!(updates.iter().any(|frame| {
            let update = &frame["params"]["update"];
            update["sessionUpdate"] == "tool_call_update"
                && &update["toolCallId"] == approval_id
                && update["status"] == if allowed { "completed" } else { "failed" }
        }));
        assert_eq!(dir.path().join("marker.txt").exists(), allowed);
        peer.close();
        assert_eq!(server.join().unwrap().len(), 2);
    }
}

#[test]
fn acp_validates_inline_images_and_passes_them_to_the_model() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("models.json"), r#"{"models":{"test-model":{"provider":"local","request_id":"test-model","metadata":"ollama/test-model"}},"overrides":{"ollama":{"models":{"test-model":{"tool_call":true,"reasoning":false,"modalities":{"input":["text","image"],"output":["text"]}}}}}}"#).unwrap();
    let (endpoint, server) =
        serve_responses(vec![sse_reply(json!({"content":"image accepted"}), "stop")]);
    let mut peer = AcpProcess::start(dir.path(), &endpoint, &[]);
    let id = peer.new_session(dir.path());
    peer.send(json!({"id":3,"method":"session/prompt","params":{"sessionId":id,"prompt":[{"type":"image","mimeType":"image/jpeg","data":"iVBORw0KGgo="}]}}));
    assert!(peer.response(3).0.get("error").is_some());
    peer.send(json!({"id":4,"method":"session/prompt","params":{"sessionId":id,"prompt":[{"type":"image","mimeType":"image/png","data":"iVBORw0KGgo="}]}}));
    let (result, _) = peer.response(4);
    assert_eq!(result["result"]["stopReason"], "end_turn", "{result}");
    peer.close();
    let requests = server.join().unwrap();
    assert!(
        requests[0]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| {
                message["content"].as_array().is_some_and(|parts| {
                    parts.iter().any(|part| {
                        part["image_url"]["url"] == "data:image/png;base64,iVBORw0KGgo="
                    })
                })
            })
    );
}

#[test]
fn acp_answers_multiple_questions_through_forms_or_choice_fallback() {
    use serde_json::json;
    for forms in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let questions = json!({"questions":[
            {"id":"features","header":"Features","question":"Which features?", "multiple":true, "allow_custom":true,
             "options":[{"id":"one","label":"One","description":"First feature"},{"id":"two","label":"Two","description":"Second feature"}]},
            {"id":"mode","header":"Mode","question":"Which mode?", "allow_custom":true,
             "options":[{"id":"plan","label":"Plan","description":"Planning"},{"id":"build","label":"Build","description":"Build changes"}]}
        ]});
        let (endpoint, server) = serve_responses(vec![
            sse_reply(
                json!({"tool_calls":[{"index":0,"id":"question-1","type":"function","function":{"name":"question","arguments":questions.to_string()}}]}),
                "tool_calls",
            ),
            sse_reply(json!({"content":"thanks"}), "stop"),
        ]);
        let mut peer = AcpProcess::start_with_capabilities(
            dir.path(),
            &endpoint,
            &[],
            if forms {
                json!({"elicitation":{"form":{}}})
            } else {
                json!({})
            },
        );
        let id = peer.new_session(dir.path());
        peer.send(json!({"id":3,"method":"session/prompt","params":{"sessionId":id,"prompt":[{"type":"text","text":"ask me"}]}}));
        let method = if forms {
            "elicitation/create"
        } else {
            "session/request_permission"
        };
        let next_request = || loop {
            let frame = peer.recv();
            assert_ne!(frame["id"], 3, "{frame}");
            if frame["method"] == method {
                break frame;
            }
        };
        let request = next_request();
        if forms {
            assert_eq!(request["params"]["mode"], "form", "{request}");
            peer.send(json!({"id":request["id"],"result":{"action":"accept","content":{"choice0":["one","two"],"custom1":"custom mode"}}}));
        } else {
            peer.send(json!({"id":request["id"],"result":{"outcome":{"outcome":"selected","optionId":"choice0"}}}));
            for option in ["choice1", "choice0"] {
                let request = loop {
                    let frame = peer.recv();
                    assert_ne!(frame["id"], 3, "{frame}");
                    if frame["method"] == method {
                        break frame;
                    }
                };
                peer.send(json!({"id":request["id"],"result":{"outcome":{"outcome":"selected","optionId":option}}}));
            }
        }
        let (result, frames) = peer.response(3);
        assert_eq!(result["result"]["stopReason"], "end_turn", "{result}");
        assert!(
            frames
                .iter()
                .any(|frame| frame["params"]["update"]["content"]["text"] == "thanks")
        );
        peer.close();
        let requests = server.join().unwrap();
        let tool_result = requests[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "tool")
            .unwrap();
        assert!(
            message_contains(tool_result, r#""selected":["one","two"]"#),
            "{tool_result}"
        );
        assert!(
            message_contains(
                tool_result,
                if forms {
                    "custom mode"
                } else {
                    r#""selected":["plan"]"#
                }
            ),
            "{tool_result}"
        );
    }
}
