//! The contract every tool implements.
//!
//! A tool is a name, a description, a JSON Schema and a way to run itself. It
//! never returns a failure to its caller: anything that goes wrong becomes an
//! error [`ToolOutput`], because the model is the one that has to read the
//! problem and decide what to do next.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use serde_json::Value;
use solaris_core::ToolSpec;

/// What running a tool produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    /// Text handed back to the model.
    ///
    /// Anything the caller would want to know — that output was truncated, that
    /// a write created directories — has to be in here, because the model is the
    /// reader.
    pub text: String,
    /// Whether the tool failed. A failure is still a result: the turn goes on so
    /// the model can react to it.
    pub is_error: bool,
}

impl ToolOutput {
    /// A successful run.
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
        }
    }

    /// A failed run, with the reason.
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
        }
    }
}

/// A handle a caller can flip to ask a running tool to stop.
///
/// The app holds one of these per turn: when the user cancels, a shell command
/// in flight has to be killed rather than left running for a model that is no
/// longer listening.
#[derive(Debug, Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    /// A handle that has not been cancelled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask whatever holds a clone of this to stop.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Whether that has happened.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// What a tool needs from whoever called it.
#[derive(Debug, Clone)]
pub struct ToolContext {
    /// Directory a relative path resolves against.
    pub cwd: PathBuf,
    /// Directory a truncated shell output is written to.
    pub temp_dir: PathBuf,
    /// Set when the caller wants a long-running tool to stop.
    pub cancel: Cancel,
}

impl ToolContext {
    /// A context rooted at `cwd`, with overflow going to `temp_dir`.
    pub fn new(cwd: impl Into<PathBuf>, temp_dir: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            temp_dir: temp_dir.into(),
            cancel: Cancel::new(),
        }
    }

    /// Whether the caller has asked for a stop.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
}

/// One thing the model can ask for.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Name the model calls it by.
    fn name(&self) -> &str;

    /// One-line description, which is how the model decides to use it.
    fn description(&self) -> &str;

    /// JSON Schema for the arguments object.
    fn parameters(&self) -> Value;

    /// Run one call.
    ///
    /// Never fails: a missing file, a rejected command and unusable arguments
    /// are all error outputs rather than a broken turn.
    async fn run(&self, input: Value, ctx: &ToolContext) -> ToolOutput;

    /// The declaration this tool contributes to a request.
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(self.name(), self.description(), self.parameters())
    }
}

/// The answer a call gets when its arguments are not an object at all.
///
/// `null` arrives here when the model emitted argument text that was not valid
/// JSON; a string or an array arrives when it misread the schema. Either way the
/// model can fix it, and saying which shape was expected is what lets it.
fn not_an_object() -> ToolOutput {
    ToolOutput::error(
        "the arguments were not a JSON object — call the tool again with arguments matching its \
         schema",
    )
}

/// A required string argument.
pub fn string_arg(input: &Value, field: &str) -> Result<String, ToolOutput> {
    let Some(object) = input.as_object() else {
        return Err(not_an_object());
    };
    match object.get(field).and_then(Value::as_str) {
        Some(value) => Ok(value.to_string()),
        None => Err(ToolOutput::error(format!(
            "`{field}` is required and must be a string"
        ))),
    }
}

/// An optional string argument. An empty string counts as absent.
pub fn optional_string_arg(input: &Value, field: &str) -> Result<Option<String>, ToolOutput> {
    let Some(object) = input.as_object() else {
        return Err(not_an_object());
    };
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if value.trim().is_empty() => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(ToolOutput::error(format!("`{field}` must be a string"))),
    }
}

/// An optional whole-number argument, which must be greater than zero.
pub fn optional_count_arg(input: &Value, field: &str) -> Result<Option<usize>, ToolOutput> {
    let Some(object) = input.as_object() else {
        return Err(not_an_object());
    };
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => match value.as_u64() {
            Some(0) | None => Err(ToolOutput::error(format!(
                "`{field}` must be a whole number greater than zero"
            ))),
            Some(count) => Ok(Some(count as usize)),
        },
    }
}

/// An optional boolean argument.
pub fn optional_bool_arg(input: &Value, field: &str) -> Result<Option<bool>, ToolOutput> {
    let Some(object) = input.as_object() else {
        return Err(not_an_object());
    };
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_bool()
            .map(Some)
            .ok_or_else(|| ToolOutput::error(format!("`{field}` must be true or false"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn arguments_that_are_not_an_object_are_reported_once() {
        // `null` is what an argument string that was not valid JSON becomes.
        for input in [Value::Null, json!("path"), json!([1, 2])] {
            let error = string_arg(&input, "path").expect_err("not an object");
            assert!(error.is_error);
            assert!(error.text.contains("JSON object"), "{}", error.text);

            assert!(optional_string_arg(&input, "glob").is_err());
            assert!(optional_count_arg(&input, "limit").is_err());
            assert!(optional_bool_arg(&input, "literal").is_err());
        }
    }

    #[test]
    fn a_required_string_is_required() {
        assert_eq!(
            string_arg(&json!({ "path": "a.txt" }), "path").expect("present"),
            "a.txt"
        );
        let error = string_arg(&json!({}), "path").expect_err("missing");
        assert!(error.is_error);
        assert!(error.text.contains("path"), "{}", error.text);
        assert!(string_arg(&json!({ "path": 7 }), "path").is_err());
    }

    #[test]
    fn an_optional_string_treats_empty_as_absent() {
        assert_eq!(
            optional_string_arg(&json!({}), "glob").expect("absent"),
            None
        );
        assert_eq!(
            optional_string_arg(&json!({ "glob": "  " }), "glob").expect("blank"),
            None
        );
        assert_eq!(
            optional_string_arg(&json!({ "glob": "*.rs" }), "glob").expect("set"),
            Some("*.rs".to_string())
        );
        assert!(optional_string_arg(&json!({ "glob": 1 }), "glob").is_err());
    }

    #[test]
    fn an_optional_count_must_be_positive() {
        assert_eq!(
            optional_count_arg(&json!({}), "limit").expect("absent"),
            None
        );
        assert_eq!(
            optional_count_arg(&json!({ "limit": 5 }), "limit").expect("set"),
            Some(5)
        );
        assert!(optional_count_arg(&json!({ "limit": 0 }), "limit").is_err());
        assert!(optional_count_arg(&json!({ "limit": -1 }), "limit").is_err());
        assert!(optional_count_arg(&json!({ "limit": "5" }), "limit").is_err());
    }

    #[test]
    fn an_optional_flag_must_be_a_flag() {
        assert_eq!(
            optional_bool_arg(&json!({}), "literal").expect("absent"),
            None
        );
        assert_eq!(
            optional_bool_arg(&json!({ "literal": true }), "literal").expect("set"),
            Some(true)
        );
        assert!(optional_bool_arg(&json!({ "literal": "yes" }), "literal").is_err());
    }

    #[test]
    fn a_cancelled_handle_says_so() {
        let cancel = Cancel::new();
        assert!(!cancel.is_cancelled());
        cancel.cancel();
        assert!(cancel.is_cancelled());
        // Clones share the flag, which is the point.
        let clone = cancel.clone();
        assert!(cancel.is_cancelled());
        assert!(clone.is_cancelled());
    }
}
