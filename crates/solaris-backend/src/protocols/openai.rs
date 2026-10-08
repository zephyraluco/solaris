//! The OpenAI Chat Completions API, streamed.
//!
//! One client covers OpenAI itself and every compatible upstream — New API,
//! OpenRouter, Google's compatibility endpoint, a local Ollama or llama.cpp
//! runtime — because they all speak this shape. What they do *not* agree on is
//! which fields the request may carry, so the module also knows how to repair a
//! body after a server has named the field it dislikes.

use serde_json::{Value, json};

use solaris_core::{AgentEvent, TurnRequest, Usage};

use crate::sse::SseEvent;

/// Marker a stream ends with.
const DONE: &str = "[DONE]";

/// Delta fields that carry a model's reasoning, most common first.
const REASONING_FIELDS: [&str; 2] = ["reasoning_content", "reasoning"];

/// Build the request body for one turn.
///
/// Unlike the Anthropic wire this one keeps system messages inline: it is the
/// shape every compatible server understands.
pub fn request_body(
    request: &TurnRequest,
    model: &str,
    max_tokens: u32,
    include_usage: bool,
) -> Value {
    let mut messages = Vec::with_capacity(request.history.len() + 1);
    for message in &request.history {
        messages.push(json!({
            "role": message.role.as_str(),
            "content": message.content,
        }));
    }
    messages.push(json!({ "role": "user", "content": request.prompt }));

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
    body
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
    }

    /// Whether the server signalled the end of its stream.
    pub fn finished(&self) -> bool {
        self.finished
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
    use solaris_core::{Message, Mode};

    fn request() -> TurnRequest {
        TurnRequest {
            history: vec![
                Message::system("You are solaris, in build mode."),
                Message::user("first question"),
            ],
            prompt: "second question".to_string(),
            mode: Mode::Build,
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
}
