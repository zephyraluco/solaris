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
