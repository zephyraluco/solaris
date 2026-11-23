//! Real binary, SQLite, and HTTP transport without terminal or external credentials.
use std::process::Output;

#[path = "support/mock_backend.rs"]
mod mock_backend;
use mock_backend::{command, message_contains, serve_responses, sse_reply};

fn run(directory: &std::path::Path, endpoint: &str, args: &[&str]) -> Output {
    command(directory, endpoint, args)
        .output()
        .expect("run binary")
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

    let reply = sse_reply(serde_json::json!({"content":"ok"}), "stop");
    let (endpoint, server) = serve_responses(vec![reply.clone(), reply]);
    let output = run(
        directory.path(),
        &endpoint,
        &["--continue", "--print", "continue user fork"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(store.records(&fork_id).expect("fork records")[fork_records..]
        .iter()
        .any(|record| matches!(record, SessionRecord::Prompt { text } if text == "continue user fork")));
    assert_eq!(
        store.records(&child_id).expect("child records").len(),
        child_records
    );

    let output = run(
        directory.path(),
        &endpoint,
        &["--resume", &child_id, "--print", "resume delegated child"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(store.records(&child_id).expect("child records")[child_records..]
        .iter()
        .any(|record| matches!(record, SessionRecord::Prompt { text } if text == "resume delegated child")));
    assert_eq!(server.join().expect("server").len(), 2);
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
        &["--continue", "--print", "continue user session"],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no saved user session"));
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
    let reply = sse_reply(serde_json::json!({"content":"ok"}), "stop");
    let (endpoint, server) = serve_responses(vec![reply.clone(), reply]);
    let mut auth = AuthStore::new();
    auth.store(
        "new-api",
        Credential::Endpoint {
            base_url: endpoint.clone(),
            api_key: "test-only".into(),
        },
    );
    auth.activate("new-api");
    std::fs::write(
        directory.path().join("auth.json"),
        auth.to_json().expect("auth"),
    )
    .expect("save auth");
    for override_provider in [false, true] {
        let mut args = vec!["--fork", &source_id, "--print", "review fork"];
        if override_provider {
            args.extend(["--provider", "new-api"]);
        }
        let output = run(directory.path(), &endpoint, &args);
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
            Some(if override_provider {
                "new-api"
            } else {
                "local"
            })
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
    assert_eq!(server.join().expect("server").len(), 2);
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

#[test]
fn history_commands_do_not_load_provider_credentials() {
    let directory = tempfile::tempdir().expect("directory");
    std::fs::write(directory.path().join("auth.json"), "{invalid credentials")
        .expect("write invalid credentials");

    let output = run(directory.path(), "http://127.0.0.1:1/v1", &["--sessions"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
}
