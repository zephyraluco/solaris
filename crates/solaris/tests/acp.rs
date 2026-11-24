//! Real ACP stdio transport with a local model fixture.
use std::io::Write;
use std::time::Duration;

#[path = "support/mock_backend.rs"]
mod mock_backend;
use mock_backend::{command, message_contains, serve_responses, sse_reply};

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
    let mode_update = peer.response(3).0;
    assert_eq!(
        mode_update["result"]["configOptions"][0]["currentValue"],
        "plan"
    );
    let mode_ids: Vec<&str> = mode_update["result"]["configOptions"][0]["options"]
        .as_array()
        .expect("mode options")
        .iter()
        .filter_map(|option| option["value"].as_str())
        .collect();
    assert_eq!(mode_ids, ["default", "plan", "auto", "ask"]);
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
    let other_cwd = dir.path().join("other workspace");
    std::fs::create_dir(&other_cwd).unwrap();
    restored.send(json!({"id":6,"method":"session/load","params":{"sessionId":id,"cwd":other_cwd,"mcpServers":[]}}));
    let (rejected, updates) = restored.response(6);
    assert_eq!(rejected["error"]["code"], -32602, "{rejected}");
    assert!(updates.is_empty());
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
