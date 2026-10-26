//! Bounded discovery keeps large external tool catalogs out of every model request.
use async_trait::async_trait;
use serde_json::{Value, json};
use solaris_tools::{Tool, ToolContext, ToolOutput};

pub(crate) struct ToolSearch;
#[async_trait]
impl Tool for ToolSearch {
    fn name(&self) -> &str {
        "tool_search"
    }
    fn description(&self) -> &str {
        "Search external MCP tools by task, server, name or description. Matching tools become available on the next model round. Use this before calling an external tool that is not yet declared."
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"query":{"type":"string","minLength":1,"maxLength":1024},"limit":{"type":"integer","minimum":1,"maximum":8}},"required":["query"],"additionalProperties":false})
    }
    async fn run(&self, _: Value, _: &ToolContext) -> ToolOutput {
        ToolOutput::error("tool_search requires the agent runtime")
    }
}
