use crate::{BackendError, Content, Role, TurnRequest, Usage};
use rig_core::completion::{CompletionRequest, ToolDefinition, Usage as RigUsage};
use rig_core::message::{self as rig, AssistantContent, AssistantMessage, UserContent};

pub(crate) fn request(
    input: &TurnRequest,
    max_tokens: u32,
) -> Result<CompletionRequest, BackendError> {
    let mut messages = Vec::new();
    for message in &input.history {
        match message.role {
            Role::System => messages.push(rig::Message::system(message.text())),
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
                messages.push(rig::Message::Assistant(AssistantMessage::new(content)));
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
                messages.push(rig::Message::User { content });
            }
        }
    }
    if !input.prompt.is_empty() {
        messages.push(rig::Message::user(&input.prompt));
    }
    let mut request = CompletionRequest::from(messages).max_tokens(u64::from(max_tokens));
    for tool in &input.tools {
        request = request.tool(ToolDefinition::new(
            name(&tool.name)?,
            &tool.description,
            tool.parameters.clone(),
        ));
    }
    Ok(request)
}

fn name(value: &str) -> Result<rig::ToolName, BackendError> {
    rig::ToolName::new(value).map_err(|error| BackendError::new(error.to_string()))
}

pub(crate) fn usage(value: RigUsage) -> Usage {
    let read = value.cached_input_tokens.unwrap_or(0);
    let write = value.cache_creation_input_tokens.unwrap_or(0);
    let input = value
        .input_tokens
        .unwrap_or(0)
        .saturating_sub(read.saturating_add(write));
    let bounded = |tokens: u64| u32::try_from(tokens).unwrap_or(u32::MAX);
    Usage::new(bounded(input), bounded(value.output_tokens.unwrap_or(0)))
        .with_cache(bounded(read), bounded(write))
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cached_input_is_not_counted_twice() {
        let mut measured = RigUsage::new();
        measured.input_tokens = Some(1000);
        measured.output_tokens = Some(50);
        measured.cached_input_tokens = Some(700);
        measured.cache_creation_input_tokens = Some(100);
        let converted = usage(measured);
        assert_eq!(converted, Usage::new(200, 50).with_cache(700, 100));
        assert_eq!(converted.total(), 1050);
    }
}
