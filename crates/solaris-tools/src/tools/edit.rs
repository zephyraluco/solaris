//! `edit` — change part of a file by exact text replacement.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::diff::{
    Edit, apply_edits, detect_line_ending, normalize_to_lf, restore_line_endings, split_bom,
};
use crate::mutation_queue::FileMutationQueue;
use crate::path_utils;
use crate::tool::{Tool, ToolContext, ToolOutput, string_arg};

/// Replaces exact text in one file.
#[derive(Debug, Clone)]
pub struct EditTool {
    queue: Arc<FileMutationQueue>,
}

impl EditTool {
    /// An editor that serialises against `queue`.
    pub fn new(queue: Arc<FileMutationQueue>) -> Self {
        Self { queue }
    }
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Replace exact text in an existing file. Every `edits[].oldText` must match a unique, \
         non-overlapping region of the file as it is now, and is matched against the original \
         text rather than against earlier edits. Put changes to the same block, or to nearby \
         lines, in one edit instead of several overlapping ones. Keep `oldText` small but unique."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to edit (relative or absolute)",
                },
                "edits": {
                    "type": "array",
                    "minItems": 1,
                    "description": "One or more targeted replacements, each matched against the original file",
                    "items": {
                        "type": "object",
                        "properties": {
                            "oldText": {
                                "type": "string",
                                "description": "Exact text to replace, which must occur exactly once in the file",
                            },
                            "newText": {
                                "type": "string",
                                "description": "Text to put in its place",
                            },
                        },
                        "required": ["oldText", "newText"],
                    },
                },
            },
            "required": ["path", "edits"],
        })
    }

    async fn run(&self, input: Value, ctx: &ToolContext) -> ToolOutput {
        let path = match string_arg(&input, "path") {
            Ok(path) => path,
            Err(error) => return error,
        };
        let edits = match parse_edits(&input) {
            Ok(edits) => edits,
            Err(error) => return error,
        };

        let absolute = path_utils::resolve(&ctx.cwd, &path);
        if path_utils::is_dir(&absolute).await {
            return ToolOutput::error(format!("{path} is a directory — `edit` works on a file"));
        }

        self.queue
            .with_lock(&absolute, || async {
                let raw = match tokio::fs::read(&absolute).await {
                    Ok(raw) => raw,
                    Err(error) => {
                        return ToolOutput::error(format!("could not read {path}: {error}"));
                    }
                };
                let Ok(text) = String::from_utf8(raw) else {
                    return ToolOutput::error(format!(
                        "{path} is not a UTF-8 text file — `edit` only handles text"
                    ));
                };

                // The mark is invisible, so it is kept aside: the model will
                // never include it in `oldText`, and the file's encoding has to
                // come back unchanged.
                let (bom, body) = split_bom(&text);
                let ending = detect_line_ending(body);
                let normalized = normalize_to_lf(body);

                let edit_count = edits.len();
                let updated = match apply_edits(&normalized, &edits) {
                    Ok(updated) => updated,
                    Err(error) => return ToolOutput::error(format!("could not edit {path}: {error}")),
                };

                if ctx.is_cancelled() {
                    return ToolOutput::error("the edit was cancelled before it was written");
                }

                let final_text = format!("{bom}{}", restore_line_endings(&updated, ending));
                match tokio::fs::write(&absolute, final_text.as_bytes()).await {
                    Ok(()) => ToolOutput::ok(format!("Replaced {edit_count} block(s) in {path}.")),
                    Err(error) => ToolOutput::error(format!("could not write {path}: {error}")),
                }
            })
            .await
    }
}

/// The `edits` array, with both sides normalised to `\n`.
///
/// A model that writes `\r\n` in `oldText` is describing the same file a person
/// sees, so the comparison happens on normalised text and the file's own line
/// endings are restored on the way out.
fn parse_edits(input: &Value) -> Result<Vec<Edit>, ToolOutput> {
    let Some(entries) = input.get("edits") else {
        return Err(ToolOutput::error(
            "`edits` is required and must be a non-empty array of {oldText, newText} objects",
        ));
    };
    let Some(entries) = entries.as_array() else {
        return Err(ToolOutput::error("`edits` must be an array"));
    };
    if entries.is_empty() {
        return Err(ToolOutput::error(
            "`edits` must contain at least one replacement",
        ));
    }

    let mut edits = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let old_text = entry.get("oldText").and_then(Value::as_str);
        let new_text = entry.get("newText").and_then(Value::as_str);
        let (Some(old_text), Some(new_text)) = (old_text, new_text) else {
            return Err(ToolOutput::error(format!(
                "edits[{index}] needs both `oldText` and `newText`, and both must be strings"
            )));
        };
        edits.push(Edit {
            old_text: normalize_to_lf(old_text),
            new_text: normalize_to_lf(new_text),
        });
    }
    Ok(edits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_dir;
    use std::path::Path;

    fn tool() -> EditTool {
        EditTool::new(Arc::new(FileMutationQueue::new()))
    }

    fn context(dir: &Path) -> ToolContext {
        ToolContext::new(dir, dir)
    }

    async fn read(dir: &Path, name: &str) -> String {
        tokio::fs::read_to_string(dir.join(name)).await.expect("read")
    }

    #[tokio::test]
    async fn a_replacement_is_written_back() {
        let dir = test_dir("edit-simple");
        tokio::fs::write(dir.join("a.txt"), "fn main() {}\n").await.expect("fixture");

        let output = tool()
            .run(
                json!({ "path": "a.txt", "edits": [{ "oldText": "main", "newText": "run" }] }),
                &context(&dir),
            )
            .await;

        assert!(!output.is_error, "{}", output.text);
        assert!(output.text.contains("1 block(s)"), "{}", output.text);
        assert_eq!(read(&dir, "a.txt").await, "fn run() {}\n");
    }

    #[tokio::test]
    async fn several_edits_land_in_one_pass() {
        let dir = test_dir("edit-many");
        tokio::fs::write(dir.join("a.txt"), "one two three").await.expect("fixture");

        tool()
            .run(
                json!({
                    "path": "a.txt",
                    "edits": [
                        { "oldText": "one", "newText": "1" },
                        { "oldText": "three", "newText": "3" },
                    ],
                }),
                &context(&dir),
            )
            .await;

        assert_eq!(read(&dir, "a.txt").await, "1 two 3");
    }

    #[tokio::test]
    async fn a_crlf_file_keeps_its_line_endings() {
        let dir = test_dir("edit-crlf");
        tokio::fs::write(dir.join("a.txt"), "one\r\ntwo\r\n").await.expect("fixture");

        let output = tool()
            .run(
                json!({ "path": "a.txt", "edits": [{ "oldText": "two\r\n", "newText": "TWO\r\n" }] }),
                &context(&dir),
            )
            .await;

        assert!(!output.is_error, "{}", output.text);
        assert_eq!(read(&dir, "a.txt").await, "one\r\nTWO\r\n");
    }

    #[tokio::test]
    async fn a_byte_order_mark_survives_and_is_not_part_of_the_match() {
        let dir = test_dir("edit-bom");
        tokio::fs::write(dir.join("a.txt"), "\u{feff}hello").await.expect("fixture");

        let output = tool()
            .run(
                json!({ "path": "a.txt", "edits": [{ "oldText": "hello", "newText": "goodbye" }] }),
                &context(&dir),
            )
            .await;

        assert!(!output.is_error, "{}", output.text);
        assert_eq!(read(&dir, "a.txt").await, "\u{feff}goodbye");
    }

    #[tokio::test]
    async fn an_ambiguous_edit_is_refused_with_an_explanation() {
        let dir = test_dir("edit-ambiguous");
        tokio::fs::write(dir.join("a.txt"), "x\nx\n").await.expect("fixture");

        let output = tool()
            .run(
                json!({ "path": "a.txt", "edits": [{ "oldText": "x", "newText": "y" }] }),
                &context(&dir),
            )
            .await;

        assert!(output.is_error);
        assert!(output.text.contains("2 times"), "{}", output.text);
        assert_eq!(read(&dir, "a.txt").await, "x\nx\n", "the file is untouched");
    }

    #[tokio::test]
    async fn malformed_edits_are_reported_per_entry() {
        let dir = test_dir("edit-malformed");
        tokio::fs::write(dir.join("a.txt"), "hello").await.expect("fixture");

        let output = tool()
            .run(
                json!({ "path": "a.txt", "edits": [{ "oldText": "h" }] }),
                &context(&dir),
            )
            .await;
        assert!(output.is_error);
        assert!(output.text.contains("edits[0]"), "{}", output.text);

        let output = tool()
            .run(json!({ "path": "a.txt", "edits": [] }), &context(&dir))
            .await;
        assert!(output.is_error);
        assert!(output.text.contains("at least one"), "{}", output.text);
    }

    #[tokio::test]
    async fn editing_a_missing_file_is_an_error_result() {
        let dir = test_dir("edit-missing");
        let output = tool()
            .run(
                json!({ "path": "nope.txt", "edits": [{ "oldText": "a", "newText": "b" }] }),
                &context(&dir),
            )
            .await;

        assert!(output.is_error);
        assert!(output.text.contains("could not read"), "{}", output.text);
    }

    #[test]
    fn the_declaration_requires_a_path_and_one_edit() {
        let tool = tool();
        assert_eq!(tool.name(), "edit");
        assert_eq!(tool.parameters()["properties"]["edits"]["minItems"], 1);
    }
}
