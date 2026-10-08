//! The OpenAI Responses API, streamed.
//!
//! The newer sibling of [`crate::protocols::openai`]'s Chat Completions: same
//! host and the same key, but the system prompt travels as a top-level
//! `instructions` field and the transcript is a list of input items rather than
//! a message list. Only the request shape and the stream parsing live here —
//! the transport is [`crate::http`] and the retry loop is [`crate::provider`] —
//! so everything is a pure function of its inputs and can be tested from a
//! fixture.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use solaris_core::{AgentEvent, Message, Role, TurnRequest, Usage};

use crate::protocols::{PartialCall, arguments_json};
use crate::sse::SseEvent;

/// Marker a stream ends with, for servers that send one.
///
/// The API itself ends with `response.completed`; this is for gateways that
/// bolt Chat Completions' sentinel onto the end of every stream.
const DONE: &str = "[DONE]";

/// Build the request body for one turn.
///
/// The system prompt becomes `instructions`, which is where the API documents
/// it. `store` is off: a terminal session has no business leaving its
/// transcript lying on the provider's servers. `tools` is written only when the
/// caller declared some.
pub fn request_body(request: &TurnRequest, model: &str, max_tokens: u32) -> Value {
    let mut instructions: Vec<String> = Vec::new();
    let mut input = Vec::with_capacity(request.history.len() + 1);

    for message in &request.history {
        if message.role == Role::System {
            let text = message.text();
            if !text.is_empty() {
                instructions.push(text);
            }
        } else {
            push_input(&mut input, message);
        }
    }
    // An empty prompt means the caller already put it in the history — which is
    // what a tool round trip does — so no further user message is appended.
    if !request.prompt.is_empty() {
        input.push(json!({ "role": "user", "content": request.prompt }));
    }

    let mut body = json!({
        "model": model,
        // `max_output_tokens`, not Chat Completions' `max_tokens`: this API
        // refuses the older name.
        "max_output_tokens": max_tokens,
        "input": input,
        "stream": true,
        "store": false,
    });
    if !instructions.is_empty() {
        body["instructions"] = Value::String(instructions.join("\n\n"));
    }
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(
            request
                .tools
                .iter()
                .map(|spec| {
                    json!({
                        "type": "function",
                        "name": spec.name,
                        "description": spec.description,
                        "parameters": spec.parameters,
                    })
                })
                .collect(),
        );
    }
    body
}

/// Append one history entry, as the input items this API is built from.
///
/// A call and its answer are items of their own rather than blocks inside a
/// message, so one message in the shared transcript can become several items
/// here. Calls come first, since that is the order they happened in. Text
/// travels as a message item of its own, and an empty one is dropped: it would
/// say nothing.
fn push_input(out: &mut Vec<Value>, message: &Message) {
    for call in message.tool_calls() {
        out.push(json!({
            "type": "function_call",
            "call_id": call.id,
            "name": call.name,
            "arguments": arguments_json(&call.input),
        }));
    }
    for result in message.tool_results() {
        out.push(json!({
            "type": "function_call_output",
            "call_id": result.id,
            "output": result.output,
        }));
    }

    let text = message.text();
    if !text.is_empty() {
        out.push(json!({ "role": message.role.as_str(), "content": text }));
    }
}

/// State carried across one Responses stream.
#[derive(Debug, Default)]
pub struct ResponsesStream {
    usage: Usage,
    reported_usage: bool,
    finished: bool,
    /// Tool calls by item id, which is what ties the announced item to the
    /// argument fragments that follow it.
    calls: BTreeMap<String, PartialCall>,
}

impl ResponsesStream {
    /// A parser waiting for the first event.
    pub fn new() -> Self {
        Self::default()
    }

    /// Interpret one decoded event, appending whatever it produces.
    pub fn handle(&mut self, event: &SseEvent, out: &mut Vec<AgentEvent>) {
        let payload = event.data.trim();
        if payload.is_empty() {
            return;
        }
        if payload == DONE {
            self.finished = true;
            return;
        }

        let Ok(data) = serde_json::from_str::<Value>(payload) else {
            return;
        };

        // Every event is named on the wire and names itself again in `type`, so
        // a gateway that drops the `event:` field is still readable.
        let kind = event
            .event
            .as_deref()
            .filter(|name| !name.is_empty())
            .or_else(|| data["type"].as_str())
            .unwrap_or_default();

        match kind {
            "response.output_text.delta" => {
                push_delta(out, &data["delta"], AgentEvent::TextDelta);
            }
            // Raw reasoning is the model's own words and a summary is their
            // condensed form; the transcript shows both as thinking.
            "response.reasoning_text.delta" | "response.reasoning_summary_text.delta" => {
                push_delta(out, &data["delta"], AgentEvent::ThinkingDelta);
            }
            "response.output_item.added" => {
                // The status line is what makes a long silence legible.
                if data["item"]["type"] == "reasoning" {
                    out.push(AgentEvent::Status("thinking".to_string()));
                }
                self.read_item(&data["item"]);
            }
            // The arguments of a call stream in after the item that announces it.
            "response.function_call_arguments.delta" => {
                let key = data["item_id"].as_str().unwrap_or_default();
                if let Some(call) = self.calls.get_mut(key) {
                    if let Some(delta) = data["delta"].as_str() {
                        call.arguments.push_str(delta);
                    }
                }
            }
            // A server may send the finished item instead of the deltas.
            "response.output_item.done" => self.read_item(&data["item"]),
            // `completed` is the ordinary ending. `incomplete` means the model
            // stopped early — a length cutoff — but its usage still counts.
            "response.completed" | "response.incomplete" => {
                self.read_usage(&data["response"]["usage"]);
                self.finished = true;
            }
            "response.failed" => {
                out.push(AgentEvent::Error(error_message(&data["response"]["error"])));
                self.finished = true;
            }
            "error" => {
                out.push(AgentEvent::Error(error_message(&data)));
                self.finished = true;
            }
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

    /// Remember a `function_call` item, or refresh it with a finished version.
    fn read_item(&mut self, item: &Value) {
        if item["type"] != "function_call" {
            return;
        }
        let key = item["id"].as_str().unwrap_or_default().to_string();
        let entry = self.calls.entry(key).or_default();

        // `call_id` is what the answer quotes back; the item id is only this
        // stream's handle on it.
        if let Some(id) = item["call_id"].as_str().filter(|id| !id.is_empty()) {
            entry.id = id.to_string();
        }
        if let Some(name) = item["name"].as_str().filter(|name| !name.is_empty()) {
            entry.name = name.to_string();
        }
        // `added` carries no arguments and `done` carries the finished ones, so
        // only overwrite when there is something to overwrite with — otherwise
        // the deltas would be lost.
        if let Some(arguments) = item["arguments"].as_str().filter(|text| !text.is_empty()) {
            entry.arguments = arguments.to_string();
        }
    }

    /// The usage the server reported, once it has reported any.
    pub fn usage(&self) -> Option<Usage> {
        self.reported_usage.then_some(self.usage)
    }

    /// Read the usage block the closing event carries.
    ///
    /// `input_tokens` counts cached input too, so the cached part is pulled back
    /// out to keep the four buckets additive.
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

        let input = read("input_tokens");
        let output = read("output_tokens");
        let cached = usage
            .get("input_tokens_details")
            .and_then(|details| details.get("cached_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(u32::MAX as u64) as u32;

        // A stream that reported nothing still gets an estimate from the
        // engine, so an all-zero block is not a report.
        if input == 0 && output == 0 && cached == 0 {
            return;
        }

        self.usage.input_tokens = input.saturating_sub(cached);
        self.usage.cache_read_tokens = cached;
        self.usage.output_tokens = output;
        self.reported_usage = true;
    }
}

/// Push a delta when the field holds a non-empty string.
fn push_delta(out: &mut Vec<AgentEvent>, value: &Value, make: fn(String) -> AgentEvent) {
    if let Some(text) = value.as_str().filter(|text| !text.is_empty()) {
        out.push(make(text.to_string()));
    }
}

/// The message inside a failed response or an `error` event.
fn error_message(error: &Value) -> String {
    error["message"]
        .as_str()
        .or_else(|| error.as_str())
        .unwrap_or("the provider rejected the request")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use solaris_core::{Mode, ToolCall, ToolResult, ToolSpec};

    fn request() -> TurnRequest {
        TurnRequest {
            history: vec![
                Message::system("You are solaris, in build mode."),
                Message::user("first question"),
                Message::assistant("first answer"),
            ],
            prompt: "second question".to_string(),
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
    fn replay(events: &[SseEvent]) -> (Vec<AgentEvent>, ResponsesStream) {
        let mut stream = ResponsesStream::new();
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
    fn the_system_prompt_becomes_instructions() {
        let body = request_body(&request(), "gpt-5", 4096);

        assert_eq!(body["model"], "gpt-5");
        assert_eq!(body["max_output_tokens"], 4096);
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false, "a CLI must not store transcripts");
        assert!(
            body.get("max_tokens").is_none(),
            "the Responses API refuses the Chat Completions field name"
        );

        assert_eq!(body["instructions"], "You are solaris, in build mode.");

        // Only the non-system entries, plus the new prompt, are input items.
        let input = body["input"].as_array().expect("input");
        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"], "first question");
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[2]["content"], "second question");
    }

    #[test]
    fn a_history_with_no_system_prompt_omits_instructions() {
        let mut request = request();
        request
            .history
            .retain(|message| message.role != Role::System);

        let body = request_body(&request, "gpt-5", 4096);
        assert!(body.get("instructions").is_none());
    }

    #[test]
    fn text_and_reasoning_both_stream() {
        let (events, stream) = replay(&[
            frame(
                "response.output_item.added",
                r#"{"item":{"type":"reasoning"}}"#,
            ),
            frame(
                "response.reasoning_summary_text.delta",
                r#"{"delta":"weighing options"}"#,
            ),
            frame("response.reasoning_text.delta", r#"{"delta":" in detail"}"#),
            frame("response.output_text.delta", r#"{"delta":"Hel"}"#),
            frame("response.output_text.delta", r#"{"delta":"lo"}"#),
        ]);

        assert_eq!(events[0], AgentEvent::Status("thinking".to_string()));
        assert_eq!(
            events[1],
            AgentEvent::ThinkingDelta("weighing options".to_string())
        );
        assert_eq!(
            events[2],
            AgentEvent::ThinkingDelta(" in detail".to_string())
        );
        assert_eq!(text_of(&events), "Hello");
        assert!(!stream.finished(), "nothing ended the stream yet");
        assert_eq!(stream.usage(), None);
    }

    #[test]
    fn the_closing_event_reports_usage_and_ends_the_stream() {
        let (events, stream) = replay(&[
            frame("response.output_text.delta", r#"{"delta":"hi"}"#),
            frame(
                "response.completed",
                r#"{"response":{"usage":{"input_tokens":1000,"input_tokens_details":{"cached_tokens":400},"output_tokens":500,"total_tokens":1500}}}"#,
            ),
        ]);

        assert_eq!(events[0], AgentEvent::TextDelta("hi".to_string()));
        assert!(stream.finished());
        assert_eq!(
            stream.usage(),
            Some(Usage::new(600, 500).with_cache(400, 0)),
            "cached input is counted inside input_tokens and must come back out"
        );
    }

    #[test]
    fn a_truncated_response_still_reports_its_usage() {
        let (_, stream) = replay(&[frame(
            "response.incomplete",
            r#"{"response":{"usage":{"input_tokens":10,"output_tokens":4096}}}"#,
        )]);

        assert!(stream.finished());
        assert_eq!(stream.usage(), Some(Usage::new(10, 4096)));
    }

    #[test]
    fn an_all_zero_usage_block_is_not_a_report() {
        let (_, stream) = replay(&[frame(
            "response.completed",
            r#"{"response":{"usage":{"input_tokens":0,"output_tokens":0}}}"#,
        )]);

        assert!(stream.finished());
        assert_eq!(
            stream.usage(),
            None,
            "the engine must estimate rather than bill zero"
        );
    }

    #[test]
    fn a_failed_response_becomes_an_error() {
        let (events, stream) = replay(&[frame(
            "response.failed",
            r#"{"response":{"error":{"code":"server_error","message":"the model exploded"}}}"#,
        )]);

        assert_eq!(
            events,
            vec![AgentEvent::Error("the model exploded".to_string())]
        );
        assert!(stream.finished());
    }

    #[test]
    fn an_error_event_becomes_an_error() {
        let (events, stream) = replay(&[frame(
            "error",
            r#"{"type":"error","code":"invalid_api_key","message":"bad key"}"#,
        )]);

        assert_eq!(events, vec![AgentEvent::Error("bad key".to_string())]);
        assert!(stream.finished());
    }

    #[test]
    fn an_event_without_a_name_is_read_from_its_type() {
        // A gateway may strip the `event:` field and leave only the payload.
        let (events, stream) = replay(&[
            frame(
                "",
                r#"{"type":"response.output_text.delta","delta":"still read"}"#,
            ),
            frame("", r#"{"type":"response.completed","response":{}}"#),
        ]);

        assert_eq!(text_of(&events), "still read");
        assert!(stream.finished());
    }

    #[test]
    fn a_sentinel_ends_a_stream_that_forgot_to_say_so() {
        let (_, stream) = replay(&[
            frame("response.output_text.delta", r#"{"delta":"hi"}"#),
            frame("", DONE),
        ]);

        assert!(stream.finished());
    }

    #[test]
    fn a_stream_that_says_nothing_is_left_unreported() {
        let (events, stream) = replay(&[
            frame("response.created", r#"{"type":"response.created"}"#),
            frame("response.in_progress", r#"{"type":"response.in_progress"}"#),
            frame(
                "response.output_item.added",
                r#"{"item":{"type":"message"}}"#,
            ),
        ]);

        assert!(events.is_empty(), "{events:?}");
        assert!(!stream.finished());
        assert_eq!(stream.usage(), None);
    }

    // ------------------------------------------------------------- tool calls

    #[test]
    fn declared_tools_are_flat_function_declarations() {
        let body = request_body(&request(), "gpt-5", 4096);
        assert!(
            body.get("tools").is_none(),
            "a request with no tools must carry no such field"
        );

        let mut request = request();
        request.tools = vec![ToolSpec::new(
            "read",
            "Read file contents",
            json!({ "type": "object" }),
        )];
        let body = request_body(&request, "gpt-5", 4096);

        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "read");
        assert_eq!(body["tools"][0]["description"], "Read file contents");
        assert_eq!(body["tools"][0]["parameters"]["type"], "object");
    }

    #[test]
    fn a_function_call_item_is_assembled_from_its_argument_deltas() {
        let (mut events, mut stream) = replay(&[
            frame(
                "response.output_item.added",
                r#"{"item":{"id":"item_1","type":"function_call","call_id":"call-1","name":"read","arguments":""}}"#,
            ),
            frame(
                "response.function_call_arguments.delta",
                r#"{"item_id":"item_1","delta":"{\"path\":"}"#,
            ),
            frame(
                "response.function_call_arguments.delta",
                r#"{"item_id":"item_1","delta":"\"a.txt\"}"}"#,
            ),
            frame(
                "response.output_item.done",
                r#"{"item":{"id":"item_1","type":"function_call","call_id":"call-1","name":"read","arguments":"{\"path\":\"a.txt\"}"}}"#,
            ),
            frame("response.completed", r#"{"response":{}}"#),
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
        assert_eq!(
            calls[0].id, "call-1",
            "the call_id is what the answer must quote back"
        );
        assert_eq!(calls[0].name, "read");
        assert_eq!(calls[0].input["path"], "a.txt");
    }

    #[test]
    fn a_finished_item_without_deltas_is_still_read() {
        let (mut events, mut stream) = replay(&[
            frame(
                "response.output_item.added",
                r#"{"item":{"id":"item_1","type":"function_call","call_id":"call-1","name":"ls"}}"#,
            ),
            frame(
                "response.output_item.done",
                r#"{"item":{"id":"item_1","type":"function_call","call_id":"call-1","name":"ls","arguments":"{\"path\":\".\"}"}}"#,
            ),
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
        assert_eq!(calls[0].input["path"], ".");
    }

    #[test]
    fn a_tool_exchange_becomes_a_call_item_and_an_output_item() {
        let call = ToolCall::new("call-1", "read", json!({ "path": "a.txt" }));
        let mut request = request();
        request.history.push(Message::tool_use(call.clone()));
        request
            .history
            .push(Message::tool_result(ToolResult::ok(&call, "file contents")));

        let body = request_body(&request, "gpt-5", 4096);
        let input = body["input"].as_array().expect("input");

        // user, assistant, function_call, function_call_output, prompt.
        assert_eq!(input.len(), 5);
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["call_id"], "call-1");
        assert_eq!(input[2]["name"], "read");
        assert_eq!(
            input[2]["arguments"], r#"{"path":"a.txt"}"#,
            "arguments travel as JSON text on this wire"
        );

        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["call_id"], "call-1");
        assert_eq!(input[3]["output"], "file contents");
    }

    #[test]
    fn an_empty_prompt_appends_no_further_item() {
        let mut request = request();
        request.prompt = String::new();

        let body = request_body(&request, "gpt-5", 4096);
        assert_eq!(
            body["input"].as_array().expect("input").len(),
            2,
            "the system prompt is instructions, and the prompt already lives in the history"
        );
    }
}
