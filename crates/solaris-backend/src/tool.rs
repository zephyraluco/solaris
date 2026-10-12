//! Tool declarations and invocations, shared by the app, the backend and the
//! tool layer.
//!
//! A [`ToolSpec`] is what a model is told a tool can do; a [`ToolCall`] is what
//! the model asked for; a [`ToolResult`] is what running it produced. All three
//! are plain data: nothing here knows how a call is executed, which is the tool
//! layer's business.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A tool declared to a model: what it does and the JSON Schema of its input.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// Name the model calls it by.
    pub name: String,
    /// One-line description, which is how the model decides to use it.
    pub description: String,
    /// JSON Schema for the arguments object (`{"type":"object","properties":…}`).
    pub parameters: Value,
}

impl ToolSpec {
    /// A spec with a name, a description and a schema.
    pub fn new(name: impl Into<String>, description: impl Into<String>, parameters: Value) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }
}

/// A tool invocation the model asked for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Provider-assigned id, echoed back with the result so the model can pair
    /// the two.
    pub id: String,
    /// Name of the tool to run.
    pub name: String,
    /// Arguments as the model emitted them, already parsed. The tool validates
    /// the shape it needs; a call with unusable arguments is an error result
    /// rather than a failed turn.
    pub input: Value,
}

impl ToolCall {
    /// A call with an id, a tool name and parsed arguments.
    pub fn new(id: impl Into<String>, name: impl Into<String>, input: Value) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            input,
        }
    }
}

/// What running a [`ToolCall`] produced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    /// Id of the call this answers.
    pub id: String,
    /// Name of the tool that ran, kept for display and for the Messages API,
    /// which does not echo it back on the wire.
    pub name: String,
    /// Text handed back to the model. Truncation, when it happened, is already
    /// described in here — the model is the reader.
    pub output: String,
    /// Whether the tool failed. A failure is still a result: the turn continues
    /// so the model can react to it.
    pub is_error: bool,
}

impl ToolResult {
    /// A successful result.
    pub fn ok(call: &ToolCall, output: impl Into<String>) -> Self {
        Self {
            id: call.id.clone(),
            name: call.name.clone(),
            output: output.into(),
            is_error: false,
        }
    }

    /// A failed result.
    pub fn error(call: &ToolCall, output: impl Into<String>) -> Self {
        Self {
            id: call.id.clone(),
            name: call.name.clone(),
            output: output.into(),
            is_error: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_spec_carries_the_schema_it_is_declared_with() {
        let spec = ToolSpec::new(
            "read",
            "Read file contents",
            json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
        );
        assert_eq!(spec.name, "read");
        assert_eq!(spec.parameters["properties"]["path"]["type"], "string");
    }

    #[test]
    fn results_echo_the_call_they_answer() {
        let call = ToolCall::new("call-1", "read", json!({ "path": "a.txt" }));

        let ok = ToolResult::ok(&call, "contents");
        assert_eq!(ok.id, "call-1");
        assert_eq!(ok.name, "read");
        assert!(!ok.is_error);

        let failed = ToolResult::error(&call, "no such file");
        assert_eq!(failed.id, "call-1");
        assert!(failed.is_error);
    }
}
