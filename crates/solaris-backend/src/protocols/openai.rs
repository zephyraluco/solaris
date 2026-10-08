//! The OpenAI Chat Completions API, streamed.
//!
//! One client covers OpenAI itself and every compatible upstream — New API,
//! OpenRouter, Google's compatibility endpoint, a local Ollama or llama.cpp
//! runtime — because they all speak this shape. What they do *not* agree on is
//! which fields the request may carry, so the module also knows how to repair a
//! body after a server has named the field it dislikes.

use serde_json::{Value, json};

use solaris_core::{AgentEvent, Message, TurnRequest, Usage};

use crate::protocols::{PartialCall, arguments_json};
use crate::sse::SseEvent;

/// Marker a stream ends with.
const DONE: &str = "[DONE]";

/// Delta fields that carry a model's reasoning, most common first.
const REASONING_FIELDS: [&str; 2] = ["reasoning_content", "reasoning"];

/// Build the request body for one turn.
///
/// Unlike the Anthropic wire this one keeps system messages inline: it is the
/// shape every compatible server understands. `tools` is written only when the
/// caller declared some, so a request that wants no tools carries no such field
/// for an upstream to reject.
pub fn request_body(
    request: &TurnRequest,
    model: &str,
    max_tokens: u32,
    include_usage: bool,
) -> Value {
    let mut messages = Vec::with_capacity(request.history.len() + 1);
    for message in &request.history {
        push_message(&mut messages, message);
    }
    // An empty prompt means the caller already put it in the history — which is
    // what a tool round trip does — so no further user message is appended.
    if !request.prompt.is_empty() {
        messages.push(json!({ "role": "user", "content": request.prompt }));
    }

    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "messages": messages,
        "stream": true,
    });
    if include_usage {
        // Asking for usage in the final chunk is an OpenAI extension that most
        // compatible servers implement, but not all — see [`repair_for`].
        body["stream_options"] = json!({ "include_usage": true });
    }
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(
            request
                .tools
                .iter()
                .map(|spec| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": spec.name,
                            "description": spec.description,
                            "parameters": spec.parameters,
                        },
                    })
                })
                .collect(),
        );
    }
    body
}

/// One history entry, in Chat Completions' shape.
///
/// A tool result is not a user message on this wire: each one is a `tool`
/// message naming the call it answers, which is why one message in the shared
/// transcript can become several here. Arguments travel as JSON *text*, not as
/// an object, so they are serialised back.
fn push_message(out: &mut Vec<Value>, message: &Message) {
    for result in message.tool_results() {
        out.push(json!({
            "role": "tool",
            "tool_call_id": result.id,
            "content": result.output,
        }));
    }

    let text = message.text();
    let calls: Vec<Value> = message
        .tool_calls()
        .map(|call| {
            json!({
                "id": call.id,
                "type": "function",
                "function": {
                    "name": call.name,
                    "arguments": arguments_json(&call.input),
                },
            })
        })
        .collect();

    // A message that was nothing but tool results has already been sent above.
    if calls.is_empty() && text.is_empty() && message.is_tool_results() {
        return;
    }

    let mut entry = json!({
        "role": message.role.as_str(),
        "content": text,
    });
    if !calls.is_empty() {
        entry["tool_calls"] = Value::Array(calls);
        // The canonical shape for a pure tool-call assistant turn is a null
        // `content`, not an empty string.
        if entry["content"].as_str().is_some_and(str::is_empty) {
            entry["content"] = Value::Null;
        }
    }
    out.push(entry);
}

/// A change to the request that a server asked for by rejecting it.
///
/// These exist because "OpenAI-compatible" describes the response shape, not
/// the accepted request fields. Each is applied at most once per turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Repair {
    /// Drop `stream_options.include_usage`, which older servers reject as an
    /// unknown field. The cost is losing the reported token counts.
    DropStreamOptions,
    /// Rename `max_tokens` to `max_completion_tokens`, which the reasoning
    /// models require.
    UseMaxCompletionTokens,
}

/// The repair a rejected request is asking for, if it names a field we know.
pub fn repair_for(body: &str) -> Option<Repair> {
    if body.contains("stream_options") {
        return Some(Repair::DropStreamOptions);
    }
    if body.contains("max_completion_tokens") {
        return Some(Repair::UseMaxCompletionTokens);
    }
    None
}

/// Apply `repair` to a request body.
pub fn apply(body: &mut Value, repair: Repair) {
    match repair {
        Repair::DropStreamOptions => {
            let Some(object) = body.as_object_mut() else {
                return;
            };
            object.remove("stream_options");
        }
        Repair::UseMaxCompletionTokens => {
            let Some(object) = body.as_object_mut() else {
                return;
            };
            let Some(max_tokens) = object.remove("max_tokens") else {
                return;
            };
            object.insert("max_completion_tokens".to_string(), max_tokens);
        }
    }
}

/// State carried across one OpenAI-compatible stream.
#[derive(Debug, Default)]
pub struct OpenAiStream {
    usage: Usage,
    reported_usage: bool,
    finished: bool,
    /// Tool calls by the index the server assigns, which is what ties the
    /// fragments of one call together.
    calls: Vec<PartialCall>,
}

impl OpenAiStream {
    /// A parser waiting for the first chunk.
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

        if let Some(error) = data.get("error") {
            out.push(AgentEvent::Error(error_text(error)));
            self.finished = true;
            return;
        }

        self.read_usage(&data["usage"]);

        let Some(choice) = data["choices"]
            .as_array()
            .and_then(|choices| choices.first())
        else {
            return;
        };
        let delta = &choice["delta"];

        let text = content_text(&delta["content"]);
        if !text.is_empty() {
            out.push(AgentEvent::TextDelta(text));
        }
        for field in REASONING_FIELDS {
            if let Some(reasoning) = delta[field].as_str().filter(|text| !text.is_empty()) {
                out.push(AgentEvent::ThinkingDelta(reasoning.to_string()));
                break;
            }
        }

        self.read_tool_calls(&delta["tool_calls"]);
    }

    /// Whether the server signalled the end of its stream.
    pub fn finished(&self) -> bool {
        self.finished
    }

    /// Hand over the calls this stream assembled.
    ///
    /// The caller runs this when the stream ends, so the model's whole message
    /// is in hand before anything is executed.
    pub fn flush(&mut self, out: &mut Vec<AgentEvent>) {
        for (index, call) in std::mem::take(&mut self.calls).into_iter().enumerate() {
            if let Some(call) = call.finish(format!("call_{index}")) {
                out.push(AgentEvent::ToolCall(call));
            }
        }
    }

    /// Read the `tool_calls` fragments of one delta.
    ///
    /// A call arrives as its id and name once, then its arguments as JSON text
    /// split at arbitrary boundaries, so only the arguments are appended.
    fn read_tool_calls(&mut self, calls: &Value) {
        let Some(entries) = calls.as_array() else {
            return;
        };
        for entry in entries {
            let index = entry["index"].as_u64().unwrap_or(0) as usize;
            while self.calls.len() <= index {
                self.calls.push(PartialCall::default());
            }
            let call = &mut self.calls[index];

            if let Some(id) = entry["id"].as_str().filter(|id| !id.is_empty()) {
                call.id = id.to_string();
            }
            if let Some(name) = entry["function"]["name"]
                .as_str()
                .filter(|name| !name.is_empty())
            {
                call.name = name.to_string();
            }
            if let Some(arguments) = entry["function"]["arguments"].as_str() {
                call.arguments.push_str(arguments);
            }
        }
    }

    /// The usage the server reported, once it has reported any.
    pub fn usage(&self) -> Option<Usage> {
        self.reported_usage.then_some(self.usage)
    }

    /// Read a usage block.
    ///
    /// This wire counts cached input inside `prompt_tokens`, so it is pulled
    /// back out again to keep the four buckets additive.
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

        let prompt = read("prompt_tokens");
        let output = read("completion_tokens");
        let cached = usage
            .get("prompt_tokens_details")
            .map(|details| {
                details
                    .get("cached_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .min(u32::MAX as u64) as u32
            })
            .unwrap_or(0);

        // Some servers emit an all-zero block per chunk; that is not a report.
        if prompt == 0 && output == 0 && cached == 0 {
            return;
        }

        self.usage.input_tokens = prompt.saturating_sub(cached);
        self.usage.cache_read_tokens = cached;
        self.usage.output_tokens = output;
        self.reported_usage = true;
    }
}

/// The text of a `content` field, which some servers send as parts rather than
/// as a plain string.
fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .concat(),
        _ => String::new(),
    }
}

/// The message inside an error object, which may be a string or a struct.
fn error_text(error: &Value) -> String {
    error["message"]
        .as_str()
        .or_else(|| error.as_str())
        .unwrap_or("the provider rejected the request")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use solaris_core::{Message, Mode, ToolCall, ToolResult, ToolSpec};

    fn request() -> TurnRequest {
        TurnRequest {
            history: vec![
                Message::system("You are solaris, in build mode."),
                Message::user("first question"),
            ],
            prompt: "second question".to_string(),
            mode: Mode::Build,
            tools: Vec::new(),
        }
    }

    fn data(payload: &str) -> SseEvent {
        SseEvent {
            event: None,
            data: payload.to_string(),
        }
    }

    /// Replay a recorded stream and collect everything it produces.
    fn replay(chunks: &[&str]) -> (Vec<AgentEvent>, OpenAiStream) {
        let mut stream = OpenAiStream::new();
        let mut out = Vec::new();
        for chunk in chunks {
            stream.handle(&data(chunk), &mut out);
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
    fn the_system_prompt_stays_in_the_messages() {
        let body = request_body(&request(), "gpt-5", 4096, true);

        assert_eq!(body["model"], "gpt-5");
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);

        let messages = body["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "You are solaris, in build mode.");
        assert_eq!(messages[2]["content"], "second question");
    }

    #[test]
    fn the_usage_extension_can_be_left_off() {
        let body = request_body(&request(), "llama3.2", 64, false);
        assert!(body.get("stream_options").is_none());
    }

    #[test]
    fn repairs_are_recognised_from_the_servers_own_wording() {
        let dropped = repair_for(
            r#"{"error":{"message":"Unrecognized request argument supplied: stream_options"}}"#,
        );
        assert_eq!(dropped, Some(Repair::DropStreamOptions));

        let renamed = repair_for(
            r#"{"error":{"message":"Unsupported parameter: 'max_tokens' is not supported with this model. Use 'max_completion_tokens' instead."}}"#,
        );
        assert_eq!(renamed, Some(Repair::UseMaxCompletionTokens));

        // An unrelated rejection is not something we can repair.
        assert_eq!(
            repair_for(r#"{"error":{"message":"model does not exist"}}"#),
            None
        );
    }

    #[test]
    fn applying_a_repair_rewrites_the_body() {
        let mut body = request_body(&request(), "gpt-5", 100, true);
        apply(&mut body, Repair::DropStreamOptions);
        assert!(body.get("stream_options").is_none());
        assert_eq!(body["max_tokens"], 100, "nothing else moved");

        let mut body = request_body(&request(), "gpt-5", 100, false);
        apply(&mut body, Repair::UseMaxCompletionTokens);
        assert_eq!(body["max_completion_tokens"], 100);
        assert!(body.get("max_tokens").is_none());

        // Applying one that does not fit is a no-op rather than a panic.
        apply(&mut body, Repair::UseMaxCompletionTokens);
        apply(&mut body, Repair::DropStreamOptions);
        assert_eq!(body["max_completion_tokens"], 100);
    }

    #[test]
    fn text_and_reasoning_both_stream_until_done() {
        let (events, stream) = replay(&[
            r#"{"choices":[{"delta":{"reasoning_content":"weighing it up"}}]}"#,
            r#"{"choices":[{"delta":{"content":"Hello "}}]}"#,
            r#"{"choices":[{"delta":{"content":"there"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":40}}}"#,
            DONE,
        ]);

        assert_eq!(text_of(&events), "Hello there");
        let thinking: String = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::ThinkingDelta(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(thinking, "weighing it up");

        assert!(stream.finished());
        // Cached input is reported separately, so it is subtracted from the
        // prompt count rather than double counted.
        assert_eq!(stream.usage(), Some(Usage::new(60, 7).with_cache(40, 0)));
    }

    #[test]
    fn a_reasoning_field_the_server_does_not_send_is_skipped() {
        let (events, _) = replay(&[r#"{"choices":[{"delta":{"content":"only text"}}]}"#]);
        assert_eq!(events, vec![AgentEvent::TextDelta("only text".to_string())]);
    }

    #[test]
    fn content_sent_as_parts_is_joined() {
        let (events, _) = replay(&[
            r#"{"choices":[{"delta":{"content":[{"type":"text","text":"one "},{"type":"text","text":"two"}]}}]}"#,
        ]);
        assert_eq!(text_of(&events), "one two");
    }

    #[test]
    fn an_all_zero_usage_block_is_not_a_report() {
        let (_, stream) = replay(&[
            r#"{"choices":[{"delta":{"content":"hi"}}],"usage":{"prompt_tokens":0,"completion_tokens":0}}"#,
        ]);
        assert_eq!(stream.usage(), None, "zeros are not a measurement");

        let (_, stream) =
            replay(&[r#"{"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":1}}"#]);
        assert_eq!(stream.usage(), Some(Usage::new(5, 1)));
    }

    #[test]
    fn noise_is_ignored_rather_than_fatal() {
        let (events, stream) = replay(&[
            "",
            "   ",
            ": comment",
            "not json",
            r#"{"choices":[]}"#,
            r#"{"choices":[{"delta":{}}]}"#,
        ]);
        assert!(events.is_empty(), "{events:?}");
        assert!(!stream.finished());
    }

    #[test]
    fn an_error_object_ends_the_stream() {
        let (events, stream) = replay(&[r#"{"error":{"message":"Rate limit reached"}}"#]);
        assert_eq!(
            events,
            vec![AgentEvent::Error("Rate limit reached".to_string())]
        );
        assert!(stream.finished());

        // Some servers send the message as a bare string.
        let (events, _) = replay(&[r#"{"error":"bad model"}"#]);
        assert_eq!(events, vec![AgentEvent::Error("bad model".to_string())]);
    }

    // ------------------------------------------------------------- tool calls

    #[test]
    fn declared_tools_reach_the_body_and_are_absent_when_there_are_none() {
        let body = request_body(&request(), "gpt-5", 100, false);
        assert!(
            body.get("tools").is_none(),
            "a request with no tools must carry no such field"
        );

        let mut request = request();
        request.tools = vec![ToolSpec::new(
            "read",
            "Read file contents",
            json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
        )];
        let body = request_body(&request, "gpt-5", 100, false);

        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "read");
        assert_eq!(body["tools"][0]["function"]["description"], "Read file contents");
        assert_eq!(
            body["tools"][0]["function"]["parameters"]["properties"]["path"]["type"],
            "string"
        );
    }

    #[test]
    fn tool_calls_are_assembled_from_their_fragments() {
        let (mut events, mut stream) = replay(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","function":{"name":"read","arguments":"{\"pa"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a.txt\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            DONE,
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
        assert_eq!(calls[0].id, "call-1");
        assert_eq!(calls[0].name, "read");
        assert_eq!(calls[0].input["path"], "a.txt");
    }

    #[test]
    fn two_tool_calls_keep_the_order_the_server_indexed_them_in() {
        let (mut events, mut stream) = replay(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"b","function":{"name":"ls","arguments":"{}"}},{"index":0,"id":"a","function":{"name":"read","arguments":"{}"}}]}}]}"#,
            DONE,
        ]);
        stream.flush(&mut events);

        let names: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::ToolCall(call) => Some(call.name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(names, vec!["read", "ls"]);
    }

    #[test]
    fn a_tool_call_with_no_name_is_not_reported() {
        let (mut events, mut stream) = replay(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","function":{"arguments":"{}"}}]}}]}"#,
            DONE,
        ]);
        stream.flush(&mut events);
        assert!(events.is_empty(), "{events:?}");
    }

    #[test]
    fn a_tool_exchange_becomes_a_call_and_a_tool_message() {
        let call = ToolCall::new("call-1", "read", json!({ "path": "a.txt" }));
        let mut request = request();
        request
            .history
            .push(Message::tool_use(call.clone()));
        request
            .history
            .push(Message::tool_result(ToolResult::ok(&call, "file contents")));

        let body = request_body(&request, "gpt-5", 100, false);
        let messages = body["messages"].as_array().expect("messages");

        // system, user, assistant(call), tool(result), then the new prompt.
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[2]["tool_calls"][0]["id"], "call-1");
        assert_eq!(messages[2]["tool_calls"][0]["type"], "function");
        assert_eq!(messages[2]["tool_calls"][0]["function"]["name"], "read");
        assert_eq!(
            messages[2]["tool_calls"][0]["function"]["arguments"], r#"{"path":"a.txt"}"#,
            "arguments travel as JSON text on this wire"
        );
        assert_eq!(
            messages[2]["content"],
            Value::Null,
            "a pure tool-call turn has a null content, not an empty string"
        );

        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "call-1");
        assert_eq!(messages[3]["content"], "file contents");
    }

    #[test]
    fn an_empty_prompt_appends_no_further_message() {
        let mut request = request();
        request.prompt = String::new();

        let body = request_body(&request, "gpt-5", 100, false);
        assert_eq!(
            body["messages"].as_array().expect("messages").len(),
            2,
            "the prompt already lives in the history"
        );
    }
}
