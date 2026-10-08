//! Dispatch across the three wire protocols.
//!
//! The backend knows only that it speaks a [`Wire`]; which request shape to
//! build and which parser to feed is decided here, once.

use serde_json::Value;

use solaris_core::{AgentEvent, TurnRequest, Usage};

use crate::protocols::anthropic::AnthropicStream;
use crate::protocols::openai::OpenAiStream;
use crate::protocols::responses::ResponsesStream;
use crate::sse::SseEvent;

/// The wire protocol an endpoint speaks.
///
/// Three protocols cover every platform in the catalogue, which is why there is
/// one client for all of them. Choosing which one an endpoint speaks is the
/// caller's business; this crate only has to speak it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    /// The Anthropic Messages API, streamed from `POST /v1/messages`.
    AnthropicMessages,
    /// The OpenAI Chat Completions API, streamed from `POST /chat/completions`.
    ///
    /// The compatibility floor: every OpenAI-compatible upstream speaks this —
    /// New API, OpenRouter, a local Ollama or llama.cpp runtime, and Google's
    /// compatibility endpoint.
    OpenAiChat,
    /// The OpenAI Responses API, streamed from `POST /responses`.
    ///
    /// OpenAI's current first choice for new work, and what its newer models
    /// are documented against. Not as widely implemented by compatible
    /// upstreams as [`Wire::OpenAiChat`], so it is chosen per platform rather
    /// than assumed.
    OpenAiResponses,
}

impl Wire {
    /// Whether this is one of the OpenAI protocols.
    ///
    /// They share a host, a key and a caching story, which is what makes some
    /// request fields worth sending to both or to neither.
    pub fn is_openai(self) -> bool {
        matches!(self, Wire::OpenAiChat | Wire::OpenAiResponses)
    }
}

/// Build the request body `wire` wants for one turn.
///
/// `prompt_cache_key` is sent to OpenAI alone: its caching is automatic, and
/// the key only steers which cache shard is used, so a compatible upstream that
/// does not know the field is never given the chance to reject it.
pub fn request_body(
    wire: Wire,
    request: &TurnRequest,
    model: &str,
    max_tokens: u32,
    prompt_cache_key: Option<&str>,
) -> Value {
    let mut body = match wire {
        // The Messages API takes the system prompt as a top-level field, and
        // marks its own cache breakpoint on the last message.
        Wire::AnthropicMessages => {
            crate::protocols::anthropic::request_body(request, model, max_tokens)
        }
        // The chat completions shape keeps the system prompt inline and learns
        // the token counts from a trailing usage chunk.
        Wire::OpenAiChat => {
            crate::protocols::openai::request_body(request, model, max_tokens, true)
        }
        // The Responses API hoists it into `instructions` like the Messages
        // API does, and reports usage in its closing event.
        Wire::OpenAiResponses => {
            crate::protocols::responses::request_body(request, model, max_tokens)
        }
    };

    if let Some(key) = prompt_cache_key.filter(|_| wire.is_openai()) {
        body["prompt_cache_key"] = Value::String(key.to_string());
    }

    body
}

/// The stream parser for the wire in use.
#[derive(Debug)]
pub enum WireStream {
    /// Anthropic Messages events.
    Anthropic(AnthropicStream),
    /// OpenAI-compatible chat completion chunks.
    OpenAi(OpenAiStream),
    /// OpenAI Responses events.
    Responses(ResponsesStream),
}

impl WireStream {
    /// A parser for `wire`.
    pub fn new(wire: Wire) -> Self {
        match wire {
            Wire::AnthropicMessages => Self::Anthropic(AnthropicStream::new()),
            Wire::OpenAiChat => Self::OpenAi(OpenAiStream::new()),
            Wire::OpenAiResponses => Self::Responses(ResponsesStream::new()),
        }
    }

    /// Interpret one decoded event, appending whatever it produces.
    pub fn handle(&mut self, event: &SseEvent, out: &mut Vec<AgentEvent>) {
        match self {
            Self::Anthropic(stream) => stream.handle(event, out),
            Self::OpenAi(stream) => stream.handle(event, out),
            Self::Responses(stream) => stream.handle(event, out),
        }
    }

    /// Whether the server signalled the end of its stream.
    pub fn finished(&self) -> bool {
        match self {
            Self::Anthropic(stream) => stream.finished(),
            Self::OpenAi(stream) => stream.finished(),
            Self::Responses(stream) => stream.finished(),
        }
    }

    /// The usage the server reported, once it has reported any.
    pub fn usage(&self) -> Option<Usage> {
        match self {
            Self::Anthropic(stream) => stream.usage(),
            Self::OpenAi(stream) => stream.usage(),
            Self::Responses(stream) => stream.usage(),
        }
    }

    /// Hand over the tool calls this stream assembled.
    ///
    /// A wire only knows a call is complete when the stream ends, so the caller
    /// runs this at that point — before the terminal event, so the turn's
    /// sequence reads as text, then calls, then completion.
    pub fn flush(&mut self, out: &mut Vec<AgentEvent>) {
        match self {
            Self::Anthropic(stream) => stream.flush(out),
            Self::OpenAi(stream) => stream.flush(out),
            Self::Responses(stream) => stream.flush(out),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solaris_core::{Message, Mode};

    fn request() -> TurnRequest {
        TurnRequest {
            history: vec![Message::system("You are solaris.")],
            prompt: "hello".to_string(),
            mode: Mode::Build,
            tools: Vec::new(),
        }
    }

    fn event(event: &str, data: &str) -> SseEvent {
        SseEvent {
            event: Some(event.to_string()),
            data: data.to_string(),
        }
    }

    #[test]
    fn each_wire_gets_the_request_shape_it_understands() {
        let anthropic = request_body(
            Wire::AnthropicMessages,
            &request(),
            "claude-sonnet-4-5",
            32,
            None,
        );
        assert_eq!(anthropic["system"], "You are solaris.");
        assert_eq!(anthropic["messages"].as_array().expect("messages").len(), 1);

        let openai = request_body(Wire::OpenAiChat, &request(), "gpt-5", 32, None);
        assert!(openai.get("system").is_none());
        assert_eq!(openai["messages"].as_array().expect("messages").len(), 2);
        assert_eq!(openai["stream_options"]["include_usage"], true);

        let responses = request_body(Wire::OpenAiResponses, &request(), "gpt-5", 32, None);
        assert_eq!(responses["instructions"], "You are solaris.");
        assert_eq!(responses["max_output_tokens"], 32);
        // The history's only entry was the system prompt, so the input list
        // holds nothing but the new prompt.
        let input = responses["input"].as_array().expect("input");
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"], "hello");
        assert!(responses.get("messages").is_none());
    }

    #[test]
    fn the_prompt_cache_key_reaches_only_the_openai_wires() {
        for wire in [Wire::OpenAiChat, Wire::OpenAiResponses] {
            let body = request_body(wire, &request(), "gpt-5", 32, Some("solaris"));
            assert_eq!(body["prompt_cache_key"], "solaris", "{wire:?}");
        }

        // The Messages API marks its own cache breakpoint instead, and a
        // compatible upstream must not be sent a field it might reject.
        let anthropic = request_body(
            Wire::AnthropicMessages,
            &request(),
            "claude-sonnet-4-5",
            32,
            Some("solaris"),
        );
        assert!(anthropic.get("prompt_cache_key").is_none());
    }

    #[test]
    fn no_key_is_sent_when_the_endpoint_is_not_openais() {
        let body = request_body(Wire::OpenAiChat, &request(), "gpt-5", 32, None);
        assert!(body.get("prompt_cache_key").is_none());
    }

    #[test]
    fn the_anthropic_parser_sees_through_the_enum() {
        let mut stream = WireStream::new(Wire::AnthropicMessages);
        let mut out = Vec::new();

        stream.handle(
            &event(
                "content_block_delta",
                r#"{"delta":{"type":"text_delta","text":"hi"}}"#,
            ),
            &mut out,
        );
        stream.handle(
            &event("message_stop", r#"{"type":"message_stop"}"#),
            &mut out,
        );

        assert_eq!(out, vec![AgentEvent::TextDelta("hi".to_string())]);
        assert!(stream.finished());
        assert_eq!(stream.usage(), None);
    }

    #[test]
    fn the_openai_parser_sees_through_the_enum() {
        let mut stream = WireStream::new(Wire::OpenAiChat);
        let mut out = Vec::new();

        stream.handle(
            &event("", r#"{"choices":[{"delta":{"content":"hi"}}]}"#),
            &mut out,
        );
        stream.handle(&event("", "[DONE]"), &mut out);

        assert_eq!(out, vec![AgentEvent::TextDelta("hi".to_string())]);
        assert!(stream.finished());
    }

    #[test]
    fn the_responses_parser_sees_through_the_enum() {
        let mut stream = WireStream::new(Wire::OpenAiResponses);
        let mut out = Vec::new();

        stream.handle(
            &event("response.output_text.delta", r#"{"delta":"hi"}"#),
            &mut out,
        );
        stream.handle(
            &event(
                "response.completed",
                r#"{"response":{"usage":{"input_tokens":7,"output_tokens":2}}}"#,
            ),
            &mut out,
        );

        assert_eq!(out, vec![AgentEvent::TextDelta("hi".to_string())]);
        assert!(stream.finished());
        assert_eq!(stream.usage(), Some(Usage::new(7, 2)));
    }
}
