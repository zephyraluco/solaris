//! The Anthropic Messages API, streamed.
//!
//! Request shape and stream parsing only: the transport lives in
//! [`crate::http`] and the retry loop in [`crate::provider`], so everything
//! here is a pure function of its inputs and can be tested from a fixture.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use solaris_core::{AgentEvent, Content, Message, Role, TurnRequest, Usage};

use crate::protocols::PartialCall;
use crate::sse::SseEvent;

/// API version the Messages API requires on every request.
pub const API_VERSION: &str = "2023-06-01";

/// Header the API key travels in.
pub const API_KEY_HEADER: &str = "x-api-key";

/// Build the request body for one turn.
///
/// The Messages API takes the system prompt as a top-level field rather than as
/// a message, so the history's system entries are hoisted out and joined. Tool
/// arguments travel as an object under `input`, so there is no argument text to
/// parse back. `tools` is written only when the caller declared some.
pub fn request_body(request: &TurnRequest, model: &str, max_tokens: u32) -> Value {
    let mut system: Vec<String> = Vec::new();
    let mut messages = Vec::with_capacity(request.history.len() + 1);

    for message in &request.history {
        if message.role == Role::System {
            let text = message.text();
            if !text.is_empty() {
                system.push(text);
            }
        } else {
            push_message(&mut messages, message);
        }
    }
    // An empty prompt means the caller already put it in the history — which is
    // what a tool round trip does — so no further user message is appended. A
    // non-empty one goes through the same path, so it merges with a tool result
    // that happens to be last.
    if !request.prompt.is_empty() {
        push_message(&mut messages, &Message::user(request.prompt.as_str()));
    }

    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "messages": messages,
        "stream": true,
    });
    if !system.is_empty() {
        body["system"] = Value::String(system.join("\n\n"));
    }
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(
            request
                .tools
                .iter()
                .map(|spec| {
                    json!({
                        "name": spec.name,
                        "description": spec.description,
                        "input_schema": spec.parameters,
                    })
                })
                .collect(),
        );
    }
    mark_cache_breakpoint(&mut body);
    body
}

/// Append one history entry, in the Messages API's shape.
///
/// Two rules make this more than a rename. The API refuses to repeat a role, so
/// consecutive same-role entries are merged into one message; and every
/// `tool_result` must *lead* the message that carries it, so merged results are
/// inserted at the front. Both cases arise when a turn ends on a tool call with
/// no closing text.
fn push_message(out: &mut Vec<Value>, message: &Message) {
    let blocks = content_blocks(message);
    if blocks.is_empty() {
        return;
    }

    let same_role = out
        .last()
        .is_some_and(|last| last["role"] == message.role.as_str());
    if !same_role {
        out.push(json!({
            "role": message.role.as_str(),
            "content": blocks,
        }));
        return;
    }

    let last = out.last_mut().expect("a last message was just found");
    let mut existing = last["content"].take();
    let mut existing = existing.as_array_mut().map(std::mem::take).unwrap_or_default();

    let mut results: Vec<Value> = Vec::new();
    let mut rest: Vec<Value> = Vec::new();
    for block in blocks {
        if block["type"] == "tool_result" {
            results.push(block);
        } else {
            rest.push(block);
        }
    }

    // Results first, then what was already there, then the new text.
    results.append(&mut existing);
    results.append(&mut rest);
    last["content"] = Value::Array(results);
}

/// A message's blocks, in the Messages API's shape.
///
/// An empty text block is rejected outright, so one that carries nothing is
/// dropped rather than sent.
fn content_blocks(message: &Message) -> Vec<Value> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            Content::Text { text } if text.is_empty() => None,
            Content::Text { text } => Some(json!({ "type": "text", "text": text })),
            Content::ToolUse { call } => Some(json!({
                "type": "tool_use",
                "id": call.id,
                "name": call.name,
                "input": call.input,
            })),
            Content::ToolResult { result } => {
                let mut block = json!({
                    "type": "tool_result",
                    "tool_use_id": result.id,
                    "content": result.output,
                });
                if result.is_error {
                    block["is_error"] = Value::Bool(true);
                }
                Some(block)
            }
        })
        .collect()
}

/// Ask the provider to cache the conversation up to the last block.
///
/// Prompt caching is opt-in per block, and the breakpoint caches everything
/// before it — which is exactly the system prompt and the turns already sent.
/// Without it every turn re-sends the whole conversation at the full input rate;
/// with it, the unchanged prefix is billed as a cache read from the second turn
/// on. Providers ignore the breakpoint below their minimum cacheable length, so
/// a short conversation costs nothing extra.
fn mark_cache_breakpoint(body: &mut Value) {
    let Some(messages) = body["messages"].as_array_mut() else {
        return;
    };
    let Some(message) = messages.last_mut() else {
        return;
    };

    // Every message here was built as a block list, but a caller-supplied body
    // may carry a plain string; turn that into a block rather than skipping the
    // breakpoint silently.
    if let Some(text) = message["content"].as_str().filter(|text| !text.is_empty()) {
        let text = Value::String(text.to_string());
        message["content"] = json!([{
            "type": "text",
            "text": text,
            "cache_control": { "type": "ephemeral" },
        }]);
        return;
    }

    let Some(blocks) = message["content"].as_array_mut() else {
        return;
    };

    // Every block type accepts a breakpoint, tool results included, so a turn
    // that ended on a tool call is cached just the same.
    if let Some(block) = blocks.last_mut() {
        block["cache_control"] = json!({ "type": "ephemeral" });
    }
}

/// State carried across one Anthropic stream.
#[derive(Debug, Default)]
pub struct AnthropicStream {
    usage: Usage,
    reported_usage: bool,
    finished: bool,
    /// Tool calls by content-block index, which is what ties the block opened by
    /// `content_block_start` to the argument fragments that follow it.
    calls: BTreeMap<u64, PartialCall>,
}

impl AnthropicStream {
    /// A parser waiting for the first event.
    pub fn new() -> Self {
        Self::default()
    }

    /// Interpret one decoded event, appending whatever it produces.
    pub fn handle(&mut self, event: &SseEvent, out: &mut Vec<AgentEvent>) {
        let Ok(data) = serde_json::from_str::<Value>(&event.data) else {
            return;
        };

        match event.event.as_deref().unwrap_or_default() {
            "message_start" => self.read_usage(&data["message"]["usage"]),
            "content_block_start" => {
                let block = &data["content_block"];
                match block["type"].as_str() {
                    Some("thinking") => out.push(AgentEvent::Status("thinking".to_string())),
                    // A tool call opens with its id and name; its arguments
                    // arrive as `input_json_delta` fragments.
                    Some("tool_use") => {
                        self.calls.insert(
                            data["index"].as_u64().unwrap_or(0),
                            PartialCall {
                                id: block["id"].as_str().unwrap_or_default().to_string(),
                                name: block["name"].as_str().unwrap_or_default().to_string(),
                                arguments: String::new(),
                            },
                        );
                    }
                    _ => {}
                }
            }
            "content_block_delta" => match data["delta"]["type"].as_str() {
                Some("text_delta") => {
                    push_delta(out, &data["delta"]["text"], AgentEvent::TextDelta)
                }
                Some("thinking_delta") => {
                    push_delta(out, &data["delta"]["thinking"], AgentEvent::ThinkingDelta)
                }
                Some("input_json_delta") => {
                    let index = data["index"].as_u64().unwrap_or(0);
                    if let Some(call) = self.calls.get_mut(&index) {
                        if let Some(fragment) = data["delta"]["partial_json"].as_str() {
                            call.arguments.push_str(fragment);
                        }
                    }
                }
                // Thinking signatures are of no use to a text transcript.
                _ => {}
            },
            "message_delta" => {
                // The closing usage block carries the output count; the cache
                // counts only ever appear in `message_start`.
                self.merge_output(&data["usage"]);
            }
            "message_stop" => self.finished = true,
            "error" => out.push(AgentEvent::Error(error_message(&data))),
            // `ping` keeps the connection alive and `content_block_stop` closes
            // a block; neither carries anything the transcript shows.
            _ => {}
        }
    }

    /// Whether the server signalled the end of its stream.
    pub fn finished(&self) -> bool {
        self.finished
    }

    /// Hand over the calls this stream assembled.
    ///
    /// Run when the stream ends, so the model's whole message is in hand before
    /// anything is executed.
    pub fn flush(&mut self, out: &mut Vec<AgentEvent>) {
        for (index, (_, call)) in std::mem::take(&mut self.calls).into_iter().enumerate() {
            if let Some(call) = call.finish(format!("call_{index}")) {
                out.push(AgentEvent::ToolCall(call));
            }
        }
    }

    /// The usage the server reported, once it has reported any.
    pub fn usage(&self) -> Option<Usage> {
        self.reported_usage.then_some(self.usage)
    }

    fn read_usage(&mut self, usage: &Value) {
        let Some(usage) = usage.as_object() else {
            return;
        };
        let read = |field: &str| {
            usage
                .get(field)
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .min(u32::MAX as u64) as u32
        };

        self.usage.input_tokens = read("input_tokens");
        self.usage.cache_read_tokens = read("cache_read_input_tokens");
        self.usage.cache_write_tokens = read("cache_creation_input_tokens");
        self.usage.output_tokens = read("output_tokens");
        self.reported_usage = self.usage.total() > 0;
    }

    fn merge_output(&mut self, usage: &Value) {
        let Some(output) = usage.get("output_tokens").and_then(Value::as_u64) else {
            return;
        };
        self.usage.output_tokens = output.min(u32::MAX as u64) as u32;
        self.reported_usage = true;
    }
}

/// Push a delta when the field holds a non-empty string.
fn push_delta(out: &mut Vec<AgentEvent>, value: &Value, make: fn(String) -> AgentEvent) {
    if let Some(text) = value.as_str().filter(|text| !text.is_empty()) {
        out.push(make(text.to_string()));
    }
}

/// The message inside an `error` event, or a generic fallback.
fn error_message(data: &Value) -> String {
    data["error"]["message"]
        .as_str()
        .unwrap_or("the model rejected the request")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use solaris_core::{Mode, ToolCall, ToolResult, ToolSpec};

    fn request(prompt: &str) -> TurnRequest {
        TurnRequest {
            history: vec![
                Message::system("You are solaris, in build mode."),
                Message::user("first question"),
                Message::assistant("first answer"),
            ],
            prompt: prompt.to_string(),
            mode: Mode::Build,
            tools: Vec::new(),
        }
    }

    fn frame(event: &str, data: &str) -> SseEvent {
        SseEvent {
            event: Some(event.to_string()),
            data: data.to_string(),
        }
    }

    /// Replay a recorded stream and collect everything it produces.
    fn replay(events: &[SseEvent]) -> (Vec<AgentEvent>, AnthropicStream) {
        let mut stream = AnthropicStream::new();
        let mut out = Vec::new();
        for event in events {
            stream.handle(event, &mut out);
        }
        (out, stream)
    }

    fn text_of(events: &[AgentEvent]) -> String {
        events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::TextDelta(text) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_system_prompt_is_hoisted_out_of_the_messages() {
        let body = request_body(&request("second question"), "claude-sonnet-4-5", 1024);

        assert_eq!(body["system"], "You are solaris, in build mode.");
        assert_eq!(body["model"], "claude-sonnet-4-5");
        assert_eq!(body["max_tokens"], 1024);
        assert_eq!(body["stream"], true);

        let messages = body["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 3, "system must not be a message");
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"][0]["text"], "first question");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(
            messages[2]["content"][0]["text"], "second question",
            "the last message carries the cache breakpoint, so its text sits in a block"
        );
    }

    #[test]
    fn the_last_message_asks_for_a_cache_breakpoint() {
        let body = request_body(&request("second question"), "claude-sonnet-4-5", 1024);

        let messages = body["messages"].as_array().expect("messages");
        let last = &messages[2];
        assert_eq!(last["content"][0]["type"], "text");
        assert_eq!(last["content"][0]["cache_control"]["type"], "ephemeral");

        // Only the last one: a breakpoint on every message would spend the
        // four the API allows and cache nothing worth re-reading.
        assert_eq!(messages[0]["content"][0]["text"], "first question");
        assert_eq!(messages[1]["content"][0]["text"], "first answer");
    }

    #[test]
    fn a_single_message_turn_still_gets_a_breakpoint() {
        let body = request_body(
            &TurnRequest {
                history: Vec::new(),
                prompt: "hello".to_string(),
                mode: Mode::Build,
                tools: Vec::new(),
            },
            "claude-haiku-4-5",
            16,
        );

        let messages = body["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
    }

    #[test]
    fn a_history_without_a_system_prompt_omits_the_field() {
        let body = request_body(
            &TurnRequest {
                history: Vec::new(),
                prompt: "hello".to_string(),
                mode: Mode::Plan,
                tools: Vec::new(),
            },
            "claude-haiku-4-5",
            16,
        );
        assert!(body.get("system").is_none());
        assert_eq!(body["messages"].as_array().expect("messages").len(), 1);
    }

    #[test]
    fn several_system_messages_are_joined() {
        let mut turn = request("hi");
        turn.history
            .insert(0, Message::system("Second instruction."));
        let body = request_body(&turn, "claude-sonnet-4-5", 16);
        assert_eq!(
            body["system"],
            "Second instruction.\n\nYou are solaris, in build mode."
        );
    }

    #[test]
    fn text_and_thinking_deltas_both_stream() {
        let (events, stream) = replay(&[
            frame(
                "message_start",
                r#"{"type":"message_start","message":{"usage":{"input_tokens":10,"cache_read_input_tokens":1000,"cache_creation_input_tokens":20,"output_tokens":1}}}"#,
            ),
            frame(
                "content_block_start",
                r#"{"content_block":{"type":"thinking"}}"#,
            ),
            frame(
                "content_block_delta",
                r#"{"delta":{"type":"thinking_delta","thinking":"weighing it up"}}"#,
            ),
            frame(
                "content_block_delta",
                r#"{"delta":{"type":"text_delta","text":"Hello "}}"#,
            ),
            frame(
                "content_block_delta",
                r#"{"delta":{"type":"text_delta","text":"there"}}"#,
            ),
            frame(
                "content_block_delta",
                r#"{"delta":{"type":"signature_delta","signature":"abc"}}"#,
            ),
            frame(
                "message_delta",
                r#"{"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":42}}"#,
            ),
            frame("message_stop", r#"{"type":"message_stop"}"#),
        ]);

        assert_eq!(text_of(&events), "Hello there");
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AgentEvent::Status(status) if status == "thinking"))
        );
        let thinking: String = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::ThinkingDelta(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(thinking, "weighing it up");

        assert!(stream.finished());
        assert_eq!(
            stream.usage(),
            Some(Usage::new(10, 42).with_cache(1000, 20))
        );
    }

    #[test]
    fn pings_and_empty_deltas_produce_nothing() {
        let (events, stream) = replay(&[
            frame("ping", r#"{"type":"ping"}"#),
            frame(
                "content_block_delta",
                r#"{"delta":{"type":"text_delta","text":""}}"#,
            ),
            frame("content_block_stop", r#"{"index":0}"#),
            // A frame this crate does not understand must not abort the stream.
            frame("something_new", r#"{"type":"something_new"}"#),
            frame("content_block_delta", "not json at all"),
        ]);

        assert!(events.is_empty(), "{events:?}");
        assert!(!stream.finished());
        assert_eq!(stream.usage(), None);
    }

    #[test]
    fn an_error_event_becomes_an_error_and_ends_nothing() {
        let (events, _) = replay(&[frame(
            "error",
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        )]);
        assert_eq!(events, vec![AgentEvent::Error("Overloaded".to_string())]);
    }

    #[test]
    fn an_error_without_a_message_still_says_something() {
        let (events, _) = replay(&[frame("error", r#"{"type":"error"}"#)]);
        assert_eq!(
            events,
            vec![AgentEvent::Error(
                "the model rejected the request".to_string()
            )]
        );
    }

    #[test]
    fn usage_is_reported_only_once_the_server_mentions_it() {
        let (_, stream) = replay(&[frame(
            "content_block_delta",
            r#"{"delta":{"type":"text_delta","text":"hi"}}"#,
        )]);
        assert_eq!(stream.usage(), None);

        // A block of zeros is not a measurement.
        let (_, stream) = replay(&[frame(
            "message_start",
            r#"{"message":{"usage":{"input_tokens":0,"output_tokens":0}}}"#,
        )]);
        assert_eq!(stream.usage(), None);
    }

    // ------------------------------------------------------------- tool calls

    #[test]
    fn declared_tools_reach_the_body_under_input_schema() {
        let body = request_body(&request("hi"), "claude-sonnet-4-5", 16);
        assert!(
            body.get("tools").is_none(),
            "a request with no tools must carry no such field"
        );

        let mut request = request("hi");
        request.tools = vec![ToolSpec::new(
            "read",
            "Read file contents",
            json!({ "type": "object" }),
        )];
        let body = request_body(&request, "claude-sonnet-4-5", 16);

        assert_eq!(body["tools"][0]["name"], "read");
        assert_eq!(body["tools"][0]["description"], "Read file contents");
        assert_eq!(
            body["tools"][0]["input_schema"]["type"], "object",
            "this API calls the schema `input_schema`"
        );
    }

    #[test]
    fn a_tool_use_block_is_assembled_from_its_argument_fragments() {
        let (mut events, mut stream) = replay(&[
            frame(
                "content_block_start",
                r#"{"index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"read"}}"#,
            ),
            frame(
                "content_block_delta",
                r#"{"index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
            ),
            frame(
                "content_block_delta",
                r#"{"index":1,"delta":{"type":"input_json_delta","partial_json":"\"a.txt\"}"}}"#,
            ),
            // A fragment for a block that was never opened is ignored.
            frame(
                "content_block_delta",
                r#"{"index":9,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
            ),
            frame("message_stop", r#"{"type":"message_stop"}"#),
        ]);
        stream.flush(&mut events);

        let calls: Vec<&ToolCall> = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::ToolCall(call) => Some(call),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "toolu_1");
        assert_eq!(calls[0].name, "read");
        assert_eq!(calls[0].input["path"], "a.txt");
    }

    #[test]
    fn a_tool_exchange_merges_into_the_messages_around_it() {
        let call = ToolCall::new("toolu_1", "read", json!({ "path": "a.txt" }));
        let mut request = request("keep going");
        request.history.push(Message::tool_use(call.clone()));
        request
            .history
            .push(Message::tool_result(ToolResult::ok(&call, "file contents")));

        let body = request_body(&request, "claude-sonnet-4-5", 100);
        let messages = body["messages"].as_array().expect("messages");

        // The API refuses to repeat a role, so the two assistant entries and
        // the tool result with the new prompt each became one message.
        assert_eq!(messages.len(), 3);

        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"][0]["text"], "first answer");
        assert_eq!(messages[1]["content"][1]["type"], "tool_use");
        assert_eq!(messages[1]["content"][1]["id"], "toolu_1");
        assert_eq!(messages[1]["content"][1]["name"], "read");
        assert_eq!(
            messages[1]["content"][1]["input"]["path"], "a.txt",
            "arguments travel as an object on this wire"
        );

        assert_eq!(messages[2]["role"], "user");
        assert_eq!(
            messages[2]["content"][0]["type"], "tool_result",
            "a tool result must lead the message that carries it"
        );
        assert_eq!(messages[2]["content"][0]["tool_use_id"], "toolu_1");
        assert_eq!(messages[2]["content"][0]["content"], "file contents");
        assert!(messages[2]["content"][0].get("is_error").is_none());
        assert_eq!(messages[2]["content"][1]["text"], "keep going");
    }

    #[test]
    fn a_failed_tool_result_is_marked_and_carries_the_breakpoint_when_last() {
        let call = ToolCall::new("toolu_1", "read", json!({}));
        let request = TurnRequest {
            history: vec![Message::tool_result(ToolResult::error(&call, "no such file"))],
            prompt: String::new(),
            mode: Mode::Build,
            tools: Vec::new(),
        };

        let body = request_body(&request, "claude-haiku-4-5", 16);
        let messages = body["messages"].as_array().expect("messages");
        assert_eq!(messages[0]["content"][0]["is_error"], true);
        assert_eq!(
            messages[0]["content"][0]["cache_control"]["type"], "ephemeral",
            "a turn that ended on a tool call is still cacheable"
        );
    }

    #[test]
    fn an_empty_prompt_appends_no_further_message() {
        let mut request = request("");
        let body = request_body(&request, "claude-sonnet-4-5", 16);
        assert_eq!(body["messages"].as_array().expect("messages").len(), 2);

        // And the breakpoint still lands on whatever is last.
        request.history.clear();
        let body = request_body(&request, "claude-sonnet-4-5", 16);
        assert!(body["messages"].as_array().expect("messages").is_empty());
    }
}
