//! Narrow native content conversion at the tool and attachment boundaries.
use rig_core::message::{ImageMediaType, ToolResultContent, UserContent};
use solaris_backend::BackendError;
use solaris_types::{InlineImage, ToolResult};
fn media_type(value: &str) -> Result<ImageMediaType, BackendError> {
    match value {
        "image/png" => Ok(ImageMediaType::PNG),
        "image/jpeg" => Ok(ImageMediaType::JPEG),
        "image/gif" => Ok(ImageMediaType::GIF),
        "image/webp" => Ok(ImageMediaType::WEBP),
        _ => Err(BackendError::new(format!(
            "unsupported image type: {value}"
        ))),
    }
}
pub(crate) fn image(image: InlineImage) -> Result<UserContent, BackendError> {
    Ok(UserContent::image_base64(
        image.data,
        Some(media_type(&image.media_type)?),
        None,
    ))
}
pub(crate) fn tool_result(result: &ToolResult) -> Result<Vec<ToolResultContent>, BackendError> {
    let mut content = vec![ToolResultContent::text(&result.output)];
    for image in &result.metadata.images {
        content.push(ToolResultContent::image_base64(
            image.data.clone(),
            Some(media_type(&image.media_type)?),
            None,
        ));
    }
    Ok(content)
}
