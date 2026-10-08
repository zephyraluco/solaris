//! The Anthropic Messages API, streamed.
//!
//! Request shape and stream parsing only: the transport lives in
//! [`crate::http`] and the retry loop in [`crate::provider`], so everything
//! here is a pure function of its inputs and can be tested from a fixture.

use serde_json::{Value, json};

use solaris_core::{AgentEvent, Message, Role, TurnRequest, Usage};

use crate::sse::SseEvent;

/// API version the Messages API requires on every request.
pub const API_VERSION: &str = "2023-06-01";

/// Header the API key travels in.
pub const API_KEY_HEADER: &str = "x-api-key";

/// Build the request body for one turn.
///
/// The Messages API takes the system prompt as a top-level field rather than as
/// a message, so the history's system entries are hoisted out and joined.
pub fn request_body(request: &TurnRequest, model: &str, max_tokens: u32) -> Value {
    let mut system: Vec<&str> = Vec::new();
    let mut messages = Vec::with_capacity(request.history.len() + 1);

    for message in &request.history {
        if message.role == Role::System {
            system.push(message.content.as_str());
        } else {
            messages.push(message_json(message));
        }
    }
    messages.push(json!({ "role": "user", "content": request.prompt }));

    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "messages": messages,
        "stream": true,
    });
    if !system.is_empty() {
        body["system"] = Value::String(system.join("\n\n"));
    }
    mark_cache_breakpoint(&mut body);
    body
}

/// Ask the provider to cache the conversation up to the last message.
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
    let Some(last) = messages.last_mut() else {
        return;
    };

    // An empty text block is rejected, and there is nothing worth caching in it.
    let Some(text) = last["content"].as_str().filter(|text| !text.is_empty()) else {
        return;
    };

    let role = last["role"].clone();
    let text = Value::String(text.to_string());
    *last = json!({
        "role": role,
        "content": [{
            "type": "text",
            "text": text,
            "cache_control": { "type": "ephemeral" },
        }],
    });
}

fn message_json(message: &Message) -> Value {
    json!({ "role": message.role.as_str(), "content": message.content })
}

/// State carried across one Anthropic stream.
#[derive(Debug, Default)]
pub struct AnthropicStream {
    usage: Usage,
    reported_usage: bool,
    finished: bool,
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
                if data["content_block"]["type"] == "thinking" {
                    out.push(AgentEvent::Status("thinking".to_string()));
                }
            }
            "content_block_delta" => match data["delta"]["type"].as_str() {
                Some("text_delta") => {
                    push_delta(out, &data["delta"]["text"], AgentEvent::TextDelta)
                }
                Some("thinking_delta") => {
                    push_delta(out, &data["delta"]["thinking"], AgentEvent::ThinkingDelta)
                }
                // Tool arguments and thinking signatures are of no use to a
                // text-only transcript.
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
    use solaris_core::Mode;

    fn request(prompt: &str) -> TurnRequest {
        TurnRequest {
            history: vec![
                Message::system("You are solaris, in build mode."),
                Message::user("first question"),
                Message::assistant("first answer"),
            ],
            prompt: prompt.to_string(),
            mode: Mode::Build,
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
        assert_eq!(messages[0]["content"], "first question");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(
            messages[2]["content"][0]["text"], "second question",
            "the last message carries the cache breakpoint, so its text moved into a block"
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
        assert_eq!(messages[0]["content"], "first question");
        assert_eq!(messages[1]["content"], "first answer");
    }

    #[test]
    fn a_single_message_turn_still_gets_a_breakpoint() {
        let body = request_body(
            &TurnRequest {
                history: Vec::new(),
                prompt: "hello".to_string(),
                mode: Mode::Build,
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
}
