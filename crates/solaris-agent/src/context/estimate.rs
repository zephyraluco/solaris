//! Usage fallback uses the same official native-message counter as context projection.
use rig_core::completion::{CompletionRequest, CompletionResponse};
use rig_core::message::{AssistantMessage, Message};
use rig_memory::{HeuristicTokenCounter, TokenCounter};
use solaris_backend::Usage;

pub(crate) fn request_tokens(request: &CompletionRequest) -> u32 {
    let counter = HeuristicTokenCounter::default();
    let messages = request
        .chat_history
        .iter()
        .map(|message| counter.count(message));
    let tools = request.tools.iter().map(|tool| counter.count_tool(tool));
    u32::try_from(messages.chain(tools).fold(0usize, usize::saturating_add)).unwrap_or(u32::MAX)
}

pub(crate) fn usage(input_tokens: u32, response: &CompletionResponse) -> Usage {
    if response.usage.input_tokens.is_some() || response.usage.output_tokens.is_some() {
        return Usage::from(response.usage);
    }
    let output = Message::Assistant(AssistantMessage::new(response.choice.to_vec()));
    Usage {
        input_tokens,
        output_tokens: u32::try_from(HeuristicTokenCounter::default().count(&output))
            .unwrap_or(u32::MAX),
        estimated: true,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::message::{AssistantContent, Origin, ToolName};
    fn response(usage: rig_core::completion::Usage) -> CompletionResponse {
        CompletionResponse::new(
            vec![AssistantContent::tool_call(
                "call",
                ToolName::new("read").unwrap(),
                serde_json::json!({"path":"README.md"}),
            )],
            usage,
            Origin::new("fixture", "fixture", "fixture"),
            serde_json::Value::Null,
        )
    }
    #[test]
    fn estimates_include_system_tools_and_nontext_output() {
        let request = CompletionRequest::from(vec![
            Message::system("instructions"),
            Message::user("question"),
        ]);
        let plain = request_tokens(&request);
        let request = request.tool(rig_core::completion::ToolDefinition::new(
            ToolName::new("read").unwrap(),
            "read a file",
            serde_json::json!({"type":"object"}),
        ));
        assert!(request_tokens(&request) > plain);
        let estimated = usage(request_tokens(&request), &response(Default::default()));
        assert!(estimated.estimated);
        assert!(
            estimated.output_tokens > 0,
            "tool-only output must consume estimated tokens"
        );
    }
    #[test]
    fn reported_usage_is_authoritative() {
        let mut reported = rig_core::completion::Usage::new();
        reported.input_tokens = Some(100);
        reported.output_tokens = Some(10);
        reported.cached_input_tokens = Some(20);
        let measured = usage(999, &response(reported));
        assert_eq!(measured, Usage::new(80, 10).with_cache(20, 0));
        assert!(!measured.estimated);
    }
}
