//! Images remain images across all three wire formats, including tool results.
use crate::{Content, InlineImage, Message, Role, ToolCall, ToolResult, TurnRequest, Wire};
fn image() -> InlineImage {
    InlineImage {
        media_type: "image/png".into(),
        data: "aW1hZ2U=".into(),
    }
}

#[test]
fn user_images_are_encoded_for_each_protocol() {
    let request = TurnRequest::new(
        vec![Message::with_content(
            Role::User,
            vec![Content::text("inspect"), Content::Image { image: image() }],
        )],
        "",
    );
    let body = crate::wire::request_body(Wire::AnthropicMessages, &request, "vision", 1000, None);
    assert_eq!(
        body["messages"][0]["content"][1]["source"]["media_type"],
        "image/png"
    );
    let body = crate::wire::request_body(Wire::OpenAiChat, &request, "vision", 1000, None);
    assert_eq!(
        body["messages"][0]["content"][1]["image_url"]["url"],
        "data:image/png;base64,aW1hZ2U="
    );
    let body = crate::wire::request_body(Wire::OpenAiResponses, &request, "vision", 1000, None);
    assert_eq!(body["input"][0]["content"][1]["type"], "input_image");
}

#[test]
fn tool_images_preserve_call_pairing_without_becoming_base64_text() {
    let call = ToolCall::new(
        "read-image",
        "read",
        serde_json::json!({"path":"image.png"}),
    );
    let mut result = ToolResult::ok(&call, "read image");
    result.metadata.images.push(image());
    let request = TurnRequest::new(
        vec![Message::tool_use(call), Message::tool_result(result)],
        "",
    );
    let body = crate::wire::request_body(Wire::AnthropicMessages, &request, "vision", 1000, None);
    assert_eq!(
        body["messages"][1]["content"][0]["content"][1]["type"],
        "image"
    );
    let body = crate::wire::request_body(Wire::OpenAiChat, &request, "vision", 1000, None);
    assert_eq!(body["messages"][1]["role"], "tool");
    assert_eq!(body["messages"][2]["role"], "user");
    assert_eq!(body["messages"][2]["content"][0]["type"], "image_url");
    let body = crate::wire::request_body(Wire::OpenAiResponses, &request, "vision", 1000, None);
    assert_eq!(body["input"][1]["type"], "function_call_output");
    assert_eq!(body["input"][2]["content"][0]["type"], "input_image");
}
