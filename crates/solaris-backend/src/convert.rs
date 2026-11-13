use crate::{BackendError, Content, Role};
use rig_core::message::{self as rig, AssistantContent, AssistantMessage, UserContent};

fn message_to_native(message: &crate::Message) -> Result<rig::Message, BackendError> {
    match message.role {
        Role::System => Ok(rig::Message::system(message.text())),
        Role::Assistant => {
            let mut content = Vec::new();
            for block in &message.content {
                match block {
                    Content::Text { text } => content.push(AssistantContent::text(text)),
                    Content::ToolUse { call } => content.push(AssistantContent::tool_call(
                        &call.id,
                        name(&call.name)?,
                        call.input.clone(),
                    )),
                    _ => return Err(BackendError::new("unsupported assistant content")),
                }
            }
            Ok(rig::Message::Assistant(AssistantMessage::new(content)))
        }
        Role::User => {
            let mut content = Vec::new();
            for block in &message.content {
                match block {
                    Content::Text { text } => content.push(UserContent::text(text)),
                    Content::ToolResult { result } => {
                        let value = rig::ToolResult {
                            call: rig::CallId::from_wire(&result.id),
                            name: name(&result.name)?,
                            content: tool_content(result)?,
                            is_error: result.is_error,
                        };
                        content.push(UserContent::ToolResult(value));
                    }
                    Content::Image { image } => content.push(UserContent::image_base64(
                        image.data.clone(),
                        Some(media_type(&image.media_type)?),
                        None,
                    )),
                    _ => return Err(BackendError::new("unsupported user content")),
                }
            }
            Ok(rig::Message::User { content })
        }
    }
}

fn name(value: &str) -> Result<rig::ToolName, BackendError> {
    rig::ToolName::new(value).map_err(|error| BackendError::new(error.to_string()))
}

fn media_type(value: &str) -> Result<rig::ImageMediaType, BackendError> {
    match value {
        "image/png" => Ok(rig::ImageMediaType::PNG),
        "image/jpeg" => Ok(rig::ImageMediaType::JPEG),
        "image/gif" => Ok(rig::ImageMediaType::GIF),
        "image/webp" => Ok(rig::ImageMediaType::WEBP),
        _ => Err(BackendError::new(format!(
            "unsupported image type: {value}"
        ))),
    }
}
fn tool_content(result: &crate::ToolResult) -> Result<Vec<rig::ToolResultContent>, BackendError> {
    let mut content = vec![rig::ToolResultContent::text(&result.output)];
    for image in &result.metadata.images {
        content.push(rig::ToolResultContent::image_base64(
            image.data.clone(),
            Some(media_type(&image.media_type)?),
            None,
        ));
    }
    Ok(content)
}

impl TryFrom<&crate::Message> for rig::Message {
    type Error = BackendError;
    fn try_from(message: &crate::Message) -> Result<Self, Self::Error> {
        message_to_native(message)
    }
}

// This is a display/legacy-import projection only. The native message, including
// reasoning signatures and provider metadata, stays intact in official memory.
impl From<&rig::Message> for crate::Message {
    fn from(message: &rig::Message) -> Self {
        match message {
            rig::Message::System { content } => Self::system(content),
            rig::Message::Assistant(message) => Self::with_content(
                Role::Assistant,
                message
                    .content
                    .iter()
                    .filter_map(|part| match part {
                        AssistantContent::Text(text) => Some(Content::text(&text.text)),
                        AssistantContent::ToolCall(call) => Some(Content::ToolUse {
                            call: crate::ToolCall::new(
                                call.id.wire().into_owned(),
                                call.function.name.as_str(),
                                call.function.arguments_value(),
                            ),
                        }),
                        _ => None,
                    })
                    .collect(),
            ),
            rig::Message::User { content } => Self::with_content(
                Role::User,
                content
                    .iter()
                    .filter_map(|part| match part {
                        UserContent::Text(text) => Some(Content::text(&text.text)),
                        UserContent::ToolResult(result) => {
                            let mut output = String::new();
                            let mut images = Vec::new();
                            for part in &result.content {
                                match part {
                                    rig::ToolResultContent::Text(text) => {
                                        output.push_str(&text.text)
                                    }
                                    rig::ToolResultContent::Json { value } => {
                                        output.push_str(&value.to_string())
                                    }
                                    rig::ToolResultContent::Image(image) => {
                                        if let Some(image) = inline_image(image) {
                                            images.push(image);
                                        }
                                    }
                                }
                            }
                            let call = crate::ToolCall::new(
                                result.call.wire().into_owned(),
                                result.name.as_str(),
                                serde_json::json!({}),
                            );
                            let mut result = if result.is_error {
                                crate::ToolResult::error(&call, output)
                            } else {
                                crate::ToolResult::ok(&call, output)
                            };
                            result.metadata.images = images;
                            Some(Content::ToolResult { result })
                        }
                        UserContent::Image(image) => {
                            inline_image(image).map(|image| Content::Image { image })
                        }
                        _ => None,
                    })
                    .collect(),
            ),
        }
    }
}
fn inline_image(image: &rig::Image) -> Option<crate::InlineImage> {
    let rig::DocumentSourceKind::Base64(data) = &image.data else {
        return None;
    };
    let media_type = match image.media_type.as_ref()? {
        rig::ImageMediaType::PNG => "image/png",
        rig::ImageMediaType::JPEG => "image/jpeg",
        rig::ImageMediaType::GIF => "image/gif",
        rig::ImageMediaType::WEBP => "image/webp",
        _ => return None,
    };
    Some(crate::InlineImage {
        media_type: media_type.into(),
        data: data.clone(),
    })
}
