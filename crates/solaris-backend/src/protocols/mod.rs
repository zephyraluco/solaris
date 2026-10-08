//! The wire protocols, one module each.
//!
//! Each module is a pure function of its inputs — build the request body, then
//! interpret the streamed events — so every protocol can be tested from a
//! recorded fixture. The transport lives in [`crate::http`], the retry loop in
//! [`crate::provider`], and [`crate::wire`] decides which of these to call.

use serde_json::Value;
use solaris_core::ToolCall;

pub(crate) mod anthropic;
pub(crate) mod openai;
pub(crate) mod responses;

/// A tool call being assembled from the fragments a stream delivers.
///
/// Every wire streams a call in pieces — the name arrives first and the
/// arguments follow as JSON text split across events — so each parser keeps one
/// of these per call and only hands the whole thing over once the stream ends.
#[derive(Debug, Default)]
pub(crate) struct PartialCall {
    /// Provider-assigned id, or empty when the server never sent one.
    pub id: String,
    /// Tool name. Taken as written, never appended: no wire streams a name
    /// character by character, so appending would duplicate one a server
    /// repeats on every fragment.
    pub name: String,
    /// Argument text exactly as received, joined across fragments.
    pub arguments: String,
}

impl PartialCall {
    /// The finished call, or `None` when the server never named a tool.
    ///
    /// A nameless call is not a call — some servers open a tool block and then
    /// abandon it — and reporting one would only earn an error the model cannot
    /// act on. `fallback_id` stands in when the server sent no id at all.
    pub fn finish(self, fallback_id: String) -> Option<ToolCall> {
        if self.name.is_empty() {
            return None;
        }
        let id = if self.id.is_empty() {
            fallback_id
        } else {
            self.id
        };
        Some(ToolCall::new(
            id,
            self.name,
            parse_arguments(&self.arguments),
        ))
    }
}

/// Argument text as a JSON value.
///
/// An empty string is an empty object: a tool that takes no arguments is still
/// called with `{}`. Text that is not JSON at all becomes `null`, which the tool
/// layer reports as an unusable call rather than quietly inventing an empty one.
pub(crate) fn parse_arguments(text: &str) -> Value {
    let text = text.trim();
    if text.is_empty() {
        return Value::Object(serde_json::Map::new());
    }
    serde_json::from_str(text).unwrap_or(Value::Null)
}

/// Arguments as the JSON text a request body carries.
pub(crate) fn arguments_json(input: &Value) -> String {
    serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_arguments_are_an_empty_object() {
        assert_eq!(parse_arguments(""), json!({}));
        assert_eq!(parse_arguments("   "), json!({}));
    }

    #[test]
    fn argument_text_is_parsed_as_json() {
        assert_eq!(parse_arguments(r#"{"path":"a"}"#), json!({ "path": "a" }));
        // Not JSON at all: reported as null rather than as an empty call.
        assert_eq!(parse_arguments("{not json"), Value::Null);
    }

    #[test]
    fn arguments_are_serialised_back_to_text() {
        assert_eq!(arguments_json(&json!({ "n": 1 })), r#"{"n":1}"#);
    }

    #[test]
    fn a_fragment_set_finishes_into_one_call() {
        let call = PartialCall {
            id: String::new(),
            name: "read".to_string(),
            arguments: r#"{"path":"a"}"#.to_string(),
        }
        .finish("call_0".to_string())
        .expect("a named call is a call");

        assert_eq!(call.id, "call_0");
        assert_eq!(call.name, "read");
        assert_eq!(call.input["path"], "a");
    }

    #[test]
    fn a_call_with_no_name_is_dropped() {
        assert!(
            PartialCall {
                id: "call-1".to_string(),
                name: String::new(),
                arguments: String::new(),
            }
            .finish("call_0".to_string())
            .is_none()
        );
    }
}
