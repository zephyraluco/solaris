//! `write` — create a file, or replace one.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::mutation_queue::FileMutationQueue;
use crate::path_utils;
use crate::tool::{Tool, ToolContext, ToolOutput, string_arg};

/// Writes whole files.
#[derive(Debug, Clone)]
pub struct WriteTool {
    queue: Arc<FileMutationQueue>,
}

impl WriteTool {
    /// A writer that serialises against `queue`.
    ///
    /// The queue is shared with `edit`, because both read a file's bytes and
    /// write them back: two of those overlapping lose one of the changes.
    pub fn new(queue: Arc<FileMutationQueue>) -> Self {
        Self { queue }
    }
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Write content to a file. Creates the file and any missing parent directories, and \
         replaces the file if it already exists. Use `edit` for a change to part of an existing \
         file."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to write (relative or absolute)",
                },
                "content": {
                    "type": "string",
                    "description": "Full content of the file",
                },
            },
            "required": ["path", "content"],
        })
    }

    async fn run(&self, input: Value, ctx: &ToolContext) -> ToolOutput {
        let path = match string_arg(&input, "path") {
            Ok(path) => path,
            Err(error) => return error,
        };
        let content = match string_arg(&input, "content") {
            Ok(content) => content,
            Err(error) => return error,
        };

        let absolute = path_utils::resolve(&ctx.cwd, &path);
        if path_utils::is_dir(&absolute).await {
            return ToolOutput::error(format!("{path} is a directory"));
        }

        let existed = path_utils::exists(&absolute).await;
        let bytes = content.len();

        self.queue
            .with_lock(&absolute, || async {
                if let Some(parent) = absolute.parent().filter(|parent| !parent.as_os_str().is_empty()) {
                    if let Err(error) = tokio::fs::create_dir_all(parent).await {
                        return ToolOutput::error(format!(
                            "could not create {}: {error}",
                            parent.display()
                        ));
                    }
                }
                if ctx.is_cancelled() {
                    return ToolOutput::error("the write was cancelled before it started");
                }
                match tokio::fs::write(&absolute, content.as_bytes()).await {
                    Ok(()) => ToolOutput::ok(describe(&path, bytes, existed)),
                    Err(error) => ToolOutput::error(format!("could not write {path}: {error}")),
                }
            })
            .await
    }
}

/// What to tell the model about the write that just happened.
///
/// Saying whether the file already existed matters: a model that thought it was
/// creating a file needs to know it overwrote one.
fn describe(path: &str, bytes: usize, existed: bool) -> String {
    let verb = if existed { "Replaced" } else { "Created" };
    format!("{verb} {path} ({bytes} byte(s)).")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_dir;
    use std::path::Path;

    fn tool() -> WriteTool {
        WriteTool::new(Arc::new(FileMutationQueue::new()))
    }

    fn context(dir: &Path) -> ToolContext {
        ToolContext::new(dir, dir)
    }

    #[tokio::test]
    async fn a_new_file_is_created_with_its_directories() {
        let dir = test_dir("write-new");
        let output = tool()
            .run(
                json!({ "path": "nested/deep/a.txt", "content": "hello" }),
                &context(&dir),
            )
            .await;

        assert!(!output.is_error, "{}", output.text);
        assert!(output.text.starts_with("Created"), "{}", output.text);
        assert_eq!(
            tokio::fs::read_to_string(dir.join("nested/deep/a.txt"))
                .await
                .expect("written"),
            "hello"
        );
    }

    #[tokio::test]
    async fn an_existing_file_is_replaced_and_says_so() {
        let dir = test_dir("write-replace");
        tokio::fs::write(dir.join("a.txt"), "old").await.expect("fixture");

        let output = tool()
            .run(json!({ "path": "a.txt", "content": "new" }), &context(&dir))
            .await;

        assert!(output.text.starts_with("Replaced"), "{}", output.text);
        assert_eq!(
            tokio::fs::read_to_string(dir.join("a.txt")).await.expect("read"),
            "new"
        );
    }

    #[tokio::test]
    async fn a_directory_target_is_refused() {
        let dir = test_dir("write-dir");
        let output = tool()
            .run(json!({ "path": ".", "content": "x" }), &context(&dir))
            .await;

        assert!(output.is_error);
        assert!(output.text.contains("directory"), "{}", output.text);
    }

    #[tokio::test]
    async fn a_missing_argument_is_reported_for_that_field() {
        let dir = test_dir("write-missing-content");
        let output = tool().run(json!({ "path": "a.txt" }), &context(&dir)).await;

        assert!(output.is_error);
        assert!(output.text.contains("content"), "{}", output.text);
    }

    #[test]
    fn the_declaration_requires_a_path_and_content() {
        let tool = tool();
        assert_eq!(tool.name(), "write");
        assert_eq!(tool.parameters()["required"][0], "path");
        assert_eq!(tool.parameters()["required"][1], "content");
    }
}
