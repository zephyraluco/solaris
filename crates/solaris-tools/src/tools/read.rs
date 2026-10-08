//! `read` — the contents of a file.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::path_utils;
use crate::tool::{Tool, ToolContext, ToolOutput, optional_count_arg, string_arg};
use crate::truncate::{Limits, head};

/// How much of a file one call returns.
const DESCRIPTION: &str = "Read the contents of a text file. Output is truncated to 2000 lines or 50KB \
     (whichever comes first). Use offset/limit for large files, and continue with offset until \
     the whole file has been read.";

/// Reads a file, or part of one.
#[derive(Debug, Clone)]
pub struct ReadTool {
    limits: Limits,
}

impl ReadTool {
    /// A reader with the default output budget.
    pub fn new() -> Self {
        Self {
            limits: Limits::default(),
        }
    }

    /// A reader with its own output budget.
    pub fn with_limits(limits: Limits) -> Self {
        Self { limits }
    }
}

impl Default for ReadTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to read (relative or absolute)",
                },
                "offset": {
                    "type": "integer",
                    "description": "Line number to start reading from (1-based)",
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of lines to read",
                },
            },
            "required": ["path"],
        })
    }

    async fn run(&self, input: Value, ctx: &ToolContext) -> ToolOutput {
        let path = match string_arg(&input, "path") {
            Ok(path) => path,
            Err(error) => return error,
        };
        let offset = match optional_count_arg(&input, "offset") {
            Ok(offset) => offset.unwrap_or(1),
            Err(error) => return error,
        };
        let limit = match optional_count_arg(&input, "limit") {
            Ok(limit) => limit,
            Err(error) => return error,
        };

        let absolute = path_utils::resolve(&ctx.cwd, &path);
        if path_utils::is_dir(&absolute).await {
            return ToolOutput::error(format!("{path} is a directory — use `ls` to list it"));
        }

        let bytes = match tokio::fs::read(&absolute).await {
            Ok(bytes) => bytes,
            Err(error) => {
                return ToolOutput::error(format!("could not read {path}: {error}"));
            }
        };
        let text = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => {
                return ToolOutput::error(format!(
                    "{path} is not a UTF-8 text file — `read` only handles text"
                ));
            }
        };

        if text.is_empty() {
            return ToolOutput::ok(format!("({path} is empty)"));
        }

        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len();
        if offset > total {
            return ToolOutput::ok(format!(
                "({path} has {total} line(s); line {offset} is past the end)"
            ));
        }

        let wanted = limit.unwrap_or(usize::MAX);
        let selected = lines[offset - 1..]
            .iter()
            .take(wanted)
            .copied()
            .collect::<Vec<_>>()
            .join("\n");

        let truncation = head(&selected, self.limits);
        let truncated = truncation.truncated();
        let shown = truncation.content.lines().count();
        let mut output = truncation.content;

        // A continuation hint beats a bare truncation notice: it tells the model
        // exactly which call gets the rest.
        let next = offset + shown;
        if next <= total && (truncated || shown < wanted) {
            output.push_str(&format!(
                "\n\n[{shown} line(s) shown from line {offset} of {total}; continue with offset={next}]"
            ));
        }

        ToolOutput::ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_dir;

    fn context(dir: &std::path::Path) -> ToolContext {
        ToolContext::new(dir, dir)
    }

    async fn write(dir: &std::path::Path, name: &str, contents: &str) {
        tokio::fs::write(dir.join(name), contents)
            .await
            .expect("fixture");
    }

    #[tokio::test]
    async fn a_small_file_comes_back_whole() {
        let dir = test_dir("read-small");
        write(&dir, "a.txt", "one\ntwo\nthree").await;

        let output = ReadTool::new()
            .run(json!({ "path": "a.txt" }), &context(&dir))
            .await;
        assert!(!output.is_error, "{}", output.text);
        assert_eq!(output.text, "one\ntwo\nthree");
    }

    #[tokio::test]
    async fn offset_and_limit_take_a_window() {
        let dir = test_dir("read-window");
        write(&dir, "a.txt", "one\ntwo\nthree\nfour").await;

        let output = ReadTool::new()
            .run(
                json!({ "path": "a.txt", "offset": 2, "limit": 2 }),
                &context(&dir),
            )
            .await;
        assert_eq!(output.text, "two\nthree");
    }

    #[tokio::test]
    async fn a_long_file_says_which_line_to_continue_from() {
        let dir = test_dir("read-long");
        let contents: String = (1..=100).map(|n| format!("line {n}\n")).collect();
        write(&dir, "a.txt", &contents).await;

        let tool = ReadTool::with_limits(Limits {
            max_lines: 3,
            max_bytes: 4096,
        });
        let output = tool.run(json!({ "path": "a.txt" }), &context(&dir)).await;

        assert!(
            output.text.starts_with("line 1\nline 2\nline 3\n"),
            "{}",
            output.text
        );
        assert!(
            output.text.contains("continue with offset=4"),
            "{}",
            output.text
        );
    }

    #[tokio::test]
    async fn a_missing_file_is_an_error_result_not_a_failed_turn() {
        let dir = test_dir("read-missing");
        let output = ReadTool::new()
            .run(json!({ "path": "nope.txt" }), &context(&dir))
            .await;

        assert!(output.is_error);
        assert!(output.text.contains("could not read"), "{}", output.text);
    }

    #[tokio::test]
    async fn a_directory_is_pointed_at_ls() {
        let dir = test_dir("read-dir");
        let output = ReadTool::new()
            .run(json!({ "path": "." }), &context(&dir))
            .await;
        assert!(output.is_error);
        assert!(output.text.contains("ls"), "{}", output.text);
    }

    #[tokio::test]
    async fn arguments_that_are_not_an_object_are_reported() {
        let dir = test_dir("read-bad-args");
        let output = ReadTool::new().run(Value::Null, &context(&dir)).await;
        assert!(output.is_error);
        assert!(output.text.contains("JSON object"), "{}", output.text);
    }

    #[tokio::test]
    async fn an_empty_file_says_so_rather_than_returning_nothing() {
        let dir = test_dir("read-empty");
        write(&dir, "a.txt", "").await;

        let output = ReadTool::new()
            .run(json!({ "path": "a.txt" }), &context(&dir))
            .await;
        assert!(output.text.contains("empty"), "{}", output.text);
    }

    #[tokio::test]
    async fn a_binary_file_is_refused() {
        let dir = test_dir("read-binary");
        tokio::fs::write(dir.join("a.bin"), [0xff, 0xfe, 0x00, 0x01])
            .await
            .expect("fixture");

        let output = ReadTool::new()
            .run(json!({ "path": "a.bin" }), &context(&dir))
            .await;
        assert!(output.is_error);
        assert!(output.text.contains("UTF-8"), "{}", output.text);
    }

    #[test]
    fn the_declaration_matches_the_implementation() {
        let tool = ReadTool::new();
        assert_eq!(tool.name(), "read");
        assert_eq!(tool.parameters()["required"][0], "path");
        assert_eq!(tool.spec().name, "read");
    }
}
