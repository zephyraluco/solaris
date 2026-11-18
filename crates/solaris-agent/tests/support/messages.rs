//! Builders and assertions over native Rig messages used by test fixtures.
use rig_core::message::{
    AssistantContent, AssistantMessage, CallId, ImageMediaType, Message, ToolName,
    ToolResultContent, UserContent,
};
use solaris_agent::{ToolCall, ToolResult};

pub fn tool_uses(calls: impl IntoIterator<Item = ToolCall>) -> Message {
    Message::Assistant(AssistantMessage::new(
        calls
            .into_iter()
            .map(|call| {
                AssistantContent::tool_call(call.id, ToolName::new(call.name).unwrap(), call.input)
            })
            .collect(),
    ))
}

pub trait MessageFixture {
    fn tool_use(call: ToolCall) -> Message;
    fn text(&self) -> String;
    fn tool_results(&self) -> impl Iterator<Item = ToolResult> + '_;
    fn tool_calls(&self) -> impl Iterator<Item = ToolCall> + '_;
    fn is_tool_results(&self) -> bool;
}
impl MessageFixture for Message {
    fn tool_use(call: ToolCall) -> Message {
        tool_uses([call])
    }

    fn text(&self) -> String {
        match self {
            Message::System { content } => content.clone(),
            Message::User { content } => content
                .iter()
                .filter_map(|part| match part {
                    UserContent::Text(value) => Some(value.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Message::Assistant(message) => message
                .content
                .iter()
                .filter_map(|part| match part {
                    AssistantContent::Text(value) => Some(value.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    fn tool_results(&self) -> impl Iterator<Item = ToolResult> + '_ {
        let parts = match self {
            Message::User { content } => content.as_slice(),
            _ => &[],
        };
        parts.iter().filter_map(|part| {
            let UserContent::ToolResult(result) = part else {
                return None;
            };
            let output: String = result
                .content
                .iter()
                .filter_map(|part| match part {
                    ToolResultContent::Text(text) => Some(text.text.clone()),
                    ToolResultContent::Json { value } => Some(value.to_string()),
                    _ => None,
                })
                .collect();
            let call = ToolCall::new(
                result.call.wire().into_owned(),
                result.name.as_str(),
                serde_json::Value::Null,
            );
            Some(if result.is_error {
                ToolResult::error(&call, output)
            } else {
                ToolResult::ok(&call, output)
            })
        })
    }

    fn tool_calls(&self) -> impl Iterator<Item = ToolCall> + '_ {
        let parts = match self {
            Message::Assistant(reply) => reply.content.as_slice(),
            _ => &[],
        };
        parts.iter().filter_map(|part| match part {
            AssistantContent::ToolCall(call) => Some(ToolCall::new(
                call.id.wire().into_owned(),
                call.function.name.as_str(),
                call.function.arguments_value(),
            )),
            _ => None,
        })
    }

    fn is_tool_results(&self) -> bool {
        matches!(self, Message::User { content } if !content.is_empty()
            && content.iter().all(|part| matches!(part, UserContent::ToolResult(_))))
    }
}

pub fn tool_result(result: ToolResult) -> Message {
    let mut content = vec![ToolResultContent::text(result.output)];
    content.extend(result.metadata.images.into_iter().map(|image| {
        let media = match image.media_type.as_str() {
            "image/png" => ImageMediaType::PNG,
            "image/jpeg" => ImageMediaType::JPEG,
            "image/gif" => ImageMediaType::GIF,
            "image/webp" => ImageMediaType::WEBP,
            other => panic!("unsupported fixture image type: {other}"),
        };
        ToolResultContent::image_base64(image.data, Some(media), None)
    }));
    Message::User {
        content: vec![UserContent::ToolResult(rig_core::message::ToolResult {
            call: CallId::from_wire(&result.id),
            name: ToolName::new(result.name).unwrap(),
            is_error: result.is_error,
            content,
        })],
    }
}
