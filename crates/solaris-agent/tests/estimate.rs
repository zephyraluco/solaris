#[path = "../src/context/estimate.rs"]
mod estimate;

use estimate::*;
use rig_core::completion::{CompletionRequest, CompletionResponse};
use rig_core::message::{AssistantContent, Message, Origin, ToolName};
use solaris_backend::Usage;
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
