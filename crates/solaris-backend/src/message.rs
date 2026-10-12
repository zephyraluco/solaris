//! Conversation messages shared by the app and the backend.

use serde::{Deserialize, Serialize};

use crate::tool::{ToolCall, ToolResult};

/// Who produced a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

impl Role {
    /// Lowercase identifier used for serialization and display.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One part of a message.
///
/// A turn that used tools is not one string: the assistant asks for calls and
/// the answers come back as blocks of their own, which is the shape the
/// Messages API wants. A text-only turn carries exactly one [`Content::Text`],
/// so a caller that never touches tools sees what it always saw.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    /// Plain text.
    Text { text: String },
    /// A call the assistant asked for.
    ToolUse { call: ToolCall },
    /// The answer to a call. Carried in a **user** message, which is where the
    /// Messages API requires a tool result to live.
    ToolResult { result: ToolResult },
}

impl Content {
    /// A text block.
    pub fn text(text: impl Into<String>) -> Self {
        Content::Text { text: text.into() }
    }

    /// The text this block carries, or `None` for a tool block.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Content::Text { text } => Some(text),
            _ => None,
        }
    }
}

/// A single message in the conversation history.
///
/// `Eq` is deliberately absent: tool arguments are JSON, and a JSON number is
/// a float, which has no total equality.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Content>,
}

impl Message {
    /// A message whose whole content is one text block.
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: vec![Content::text(content)],
        }
    }

    /// A message built from blocks of the caller's choosing.
    pub fn with_content(role: Role, content: Vec<Content>) -> Self {
        Self { role, content }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::new(Role::User, content)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::new(Role::Assistant, content)
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self::new(Role::System, content)
    }

    /// An assistant message that asks for `call`.
    pub fn tool_use(call: ToolCall) -> Self {
        Self::with_content(Role::Assistant, vec![Content::ToolUse { call }])
    }

    /// A user message that answers a call.
    pub fn tool_result(result: ToolResult) -> Self {
        Self::with_content(Role::User, vec![Content::ToolResult { result }])
    }

    /// The text blocks joined, which is what a text-only message reads as.
    ///
    /// This is what the system prompt is hoisted from and what a transcript
    /// line shows; a tool block contributes nothing here.
    pub fn text(&self) -> String {
        let parts: Vec<&str> = self.content.iter().filter_map(Content::as_text).collect();
        parts.join("\n")
    }

    /// Characters across every block, for the estimate fallback.
    pub fn characters(&self) -> usize {
        self.content
            .iter()
            .map(|block| match block {
                Content::Text { text } => text.chars().count(),
                Content::ToolUse { call } => {
                    call.name.chars().count() + call.input.to_string().chars().count()
                }
                Content::ToolResult { result } => result.output.chars().count(),
            })
            .sum()
    }

    /// The calls this message asks for.
    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.content.iter().filter_map(|block| match block {
            Content::ToolUse { call } => Some(call),
            _ => None,
        })
    }

    /// The results this message carries.
    pub fn tool_results(&self) -> impl Iterator<Item = &ToolResult> {
        self.content.iter().filter_map(|block| match block {
            Content::ToolResult { result } => Some(result),
            _ => None,
        })
    }

    /// Whether every block is a tool result, which is what the OpenAI wires
    /// have to split into one `tool` message per result.
    pub fn is_tool_results(&self) -> bool {
        !self.content.is_empty()
            && self
                .content
                .iter()
                .all(|block| matches!(block, Content::ToolResult { .. }))
    }

    /// Whether the message carries no block at all.
    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_text_message_reads_back_as_its_text() {
        let message = Message::user("hello");
        assert_eq!(message.role, Role::User);
        assert_eq!(message.text(), "hello");
        assert!(message.tool_calls().next().is_none());
        assert!(message.tool_results().next().is_none());
        assert!(!message.is_tool_results());
        assert_eq!(message.characters(), 5);
    }

    #[test]
    fn a_tool_use_message_carries_the_call() {
        let call = ToolCall::new("call-1", "read", json!({ "path": "a.txt" }));
        let message = Message::tool_use(call.clone());

        assert_eq!(message.role, Role::Assistant);
        assert_eq!(message.tool_calls().collect::<Vec<_>>(), vec![&call]);
        // Tool blocks contribute no display text.
        assert_eq!(message.text(), "");
    }

    #[test]
    fn a_tool_result_message_is_a_user_message_and_is_recognisable() {
        let call = ToolCall::new("call-1", "read", json!({}));
        let message = Message::tool_result(ToolResult::error(&call, "no such file"));

        assert_eq!(message.role, Role::User);
        assert!(message.is_tool_results());
        assert_eq!(message.characters(), "no such file".chars().count());
    }

    #[test]
    fn tool_characters_count_towards_the_estimate() {
        let call = ToolCall::new("call-1", "read", json!({ "path": "a" }));
        let message = Message::tool_use(call);
        assert!(message.characters() > 0);
    }

    #[test]
    fn content_blocks_round_trip_through_json() {
        let message = Message::with_content(
            Role::Assistant,
            vec![
                Content::text("looking"),
                Content::ToolUse {
                    call: ToolCall::new("call-1", "read", json!({ "path": "a" })),
                },
            ],
        );

        let encoded = serde_json::to_string(&message).expect("serialises");
        let decoded: Message = serde_json::from_str(&encoded).expect("round trips");
        assert_eq!(decoded, message);
    }
}
