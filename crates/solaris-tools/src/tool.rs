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
    /// Structured display and execution data; never parsed from text.
    pub metadata: Box<solaris_types::ToolMetadata>,
}

impl ToolOutput {
    /// A successful run.
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
            metadata: Box::default(),
        }
    }

    /// A failed run, with the reason.
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
            metadata: Box::new(solaris_types::ToolMetadata {
                status: solaris_types::ToolStatus::Failed,
                ..Default::default()
            }),
        }
    }
}

/// A handle a caller can flip to ask a running tool to stop.
///
/// The app holds one of these per turn: when the user cancels, a shell command
/// in flight has to be killed rather than left running for a model that is no
/// longer listening.
#[derive(Debug, Clone, Default)]
pub struct Cancel {
    current: Arc<std::sync::Mutex<Arc<AtomicBool>>>,
    token: Option<Arc<AtomicBool>>,
}

impl Cancel {
    /// A handle that has not been cancelled.
    pub fn new() -> Self {
        Self::default()
    }

    fn token(&self) -> Arc<AtomicBool> {
        self.token
            .clone()
            .unwrap_or_else(|| Arc::clone(&self.current.lock().unwrap_or_else(|p| p.into_inner())))
    }

    /// Stop this generation, or the active generation when used as a controller.
    pub fn cancel(&self) {
        self.token().store(true, Ordering::Release);
    }

    /// Start a new generation without reviving old workers.
    pub fn begin_turn(&self) -> Self {
        let token = Arc::new(AtomicBool::new(false));
        let mut current = self.current.lock().unwrap_or_else(|p| p.into_inner());
        current.store(true, Ordering::Release);
        *current = Arc::clone(&token);
        Self {
            current: Arc::clone(&self.current),
            token: Some(token),
        }
    }

    /// Reset the controller for callers that do not need a generation handle.
    pub fn reset(&self) {
        drop(self.begin_turn());
    }

    /// Whether this generation has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.token().load(Ordering::Acquire)
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

/// Whether a tool may run beside the other calls in its round.
///
/// Most tools are independent — two reads, or a read and a search, do not care
/// about each other — so they run together by default. A tool that changes
/// something another call in the same round might read, or that must not overlap
/// itself, declares itself [`ExecutionMode::Sequential`] instead, and the whole
/// round then runs one call at a time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ExecutionMode {
    /// May run at the same time as the round's other calls.
    #[default]
    Parallel,
    /// Runs alone: the round's other calls wait for it.
    Sequential,
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

    /// Guidance for the model about when and how to use this tool.
    ///
    /// The agent folds this into the system prompt. `description` decides
    /// *whether* the model reaches for the tool; this decides whether it uses
    /// the tool well, so a tool whose correct use is not obvious from its schema
    /// alone should answer here. `None` — the default — leaves the description
    /// as the only thing the model is told.
    ///
    /// It answers with a literal rather than a built string because it ends up
    /// in a prompt that is assembled once per turn: allocating for it on every
    /// call would buy nothing.
    fn guideline(&self) -> Option<&'static str> {
        None
    }

    /// Run one call.
    ///
    /// Never fails: a missing file, a rejected command and unusable arguments
    /// are all error outputs rather than a broken turn.
    async fn run(&self, input: Value, ctx: &ToolContext) -> ToolOutput;

    /// How this tool may run in a round with other calls.
    ///
    /// Parallel by default, which is right for anything that only reads. A tool
    /// that writes, or that runs a program whose effect another call could race,
    /// answers [`ExecutionMode::Sequential`].
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
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
            Some(count) => Ok(Some(usize::try_from(count).unwrap_or(usize::MAX))),
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
    fn a_new_turn_never_revives_an_old_cancellation_token() {
        let controller = Cancel::new();
        let old = controller.begin_turn();
        old.cancel();
        let current = controller.begin_turn();
        assert!(old.is_cancelled());
        assert!(!current.is_cancelled());
        controller.cancel();
        assert!(current.is_cancelled());
    }

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
