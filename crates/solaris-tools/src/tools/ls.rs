//! `ls` — what lives in a directory.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::path_utils;
use crate::tool::{Tool, ToolContext, ToolOutput, optional_count_arg, optional_string_arg};
use crate::truncate::{Limits, head};

/// Most entries one listing returns.
const DEFAULT_LIMIT: usize = 500;

/// Lists directories.
#[derive(Debug, Clone)]
pub struct LsTool {
    limits: Limits,
    default_limit: usize,
}

impl LsTool {
    /// A lister with the default entry budget.
    pub fn new() -> Self {
        Self {
            limits: Limits::default(),
            default_limit: DEFAULT_LIMIT,
        }
    }

    /// A lister with its own budgets.
    pub fn with_limits(limits: Limits, default_limit: usize) -> Self {
        Self {
            limits,
            default_limit,
        }
    }
}

impl Default for LsTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for LsTool {
    fn name(&self) -> &str {
        "ls"
    }

    fn description(&self) -> &str {
        "List the entries of a directory: files and directories, sorted alphabetically, with a \
         trailing `/` on directories. Dotfiles are included. Hidden ignores such as .gitignore \
         are not applied here — use `find` for that."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory to list (default: the session directory)",
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of entries to return (default: 500)",
                },
            },
        })
    }

    async fn run(&self, input: Value, ctx: &ToolContext) -> ToolOutput {
        let path = match optional_string_arg(&input, "path") {
            Ok(path) => path.unwrap_or_else(|| ".".to_string()),
            Err(error) => return error,
        };
        let limit = match optional_count_arg(&input, "limit") {
            Ok(limit) => limit.unwrap_or(self.default_limit),
            Err(error) => return error,
        };

        let absolute = path_utils::resolve(&ctx.cwd, &path);
        let mut entries = match tokio::fs::read_dir(&absolute).await {
            Ok(entries) => entries,
            Err(error) => {
                return ToolOutput::error(format!("could not list {path}: {error}"));
            }
        };

        let mut names: Vec<String> = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = entry
                .file_type()
                .await
                .map(|kind| kind.is_dir())
                .unwrap_or(false);
            names.push(if is_dir { format!("{name}/") } else { name });
        }

        if names.is_empty() {
            return ToolOutput::ok(format!("({path} is empty)"));
        }

        names.sort_by_key(|name| name.to_lowercase());

        let mut notices = Vec::new();
        if names.len() > limit {
            notices.push(format!(
                "{limit} entries limit reached; use limit={} for more",
                limit * 2
            ));
            names.truncate(limit);
        }

        let listing = names.join("\n");
        let truncation = head(&listing, self.limits);
        if let Some(notice) = truncation.notice(self.limits) {
            notices.push(notice);
        }

        let mut output = truncation.content;
        if !notices.is_empty() {
            output.push_str(&format!("\n\n[{}]", notices.join(". ")));
        }
        ToolOutput::ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_dir;
    use std::path::Path;

    fn context(dir: &Path) -> ToolContext {
        ToolContext::new(dir, dir)
    }

    #[tokio::test]
    async fn entries_are_sorted_and_directories_are_marked() {
        let dir = test_dir("ls-sorted");
        tokio::fs::create_dir_all(dir.join("src")).await.expect("fixture");
        tokio::fs::write(dir.join("README.md"), "hi").await.expect("fixture");
        tokio::fs::write(dir.join(".env"), "x").await.expect("fixture");

        let output = LsTool::new().run(json!({}), &context(&dir)).await;
        assert!(!output.is_error, "{}", output.text);

        let lines: Vec<&str> = output.text.lines().collect();
        assert_eq!(lines, vec![".env", "README.md", "src/"]);
    }

    #[tokio::test]
    async fn an_empty_directory_says_so() {
        let dir = test_dir("ls-empty");
        let output = LsTool::new().run(json!({}), &context(&dir)).await;
        assert!(output.text.contains("empty"), "{}", output.text);
    }

    #[tokio::test]
    async fn the_entry_limit_is_reported_with_a_way_past_it() {
        let dir = test_dir("ls-limit");
        for index in 0..5 {
            tokio::fs::write(dir.join(format!("f{index}")), "x")
                .await
                .expect("fixture");
        }

        let output = LsTool::new()
            .run(json!({ "limit": 2 }), &context(&dir))
            .await;

        assert_eq!(output.text.lines().filter(|l| l.starts_with('f')).count(), 2);
        assert!(output.text.contains("use limit=4"), "{}", output.text);
    }

    #[tokio::test]
    async fn a_missing_directory_is_an_error_result() {
        let dir = test_dir("ls-missing");
        let output = LsTool::new()
            .run(json!({ "path": "nope" }), &context(&dir))
            .await;

        assert!(output.is_error);
        assert!(output.text.contains("could not list"), "{}", output.text);
    }

    #[tokio::test]
    async fn a_file_is_not_a_directory() {
        let dir = test_dir("ls-file");
        tokio::fs::write(dir.join("a.txt"), "x").await.expect("fixture");

        let output = LsTool::new()
            .run(json!({ "path": "a.txt" }), &context(&dir))
            .await;
        assert!(output.is_error);
    }

    #[test]
    fn the_declaration_takes_no_required_argument() {
        let tool = LsTool::new();
        assert_eq!(tool.name(), "ls");
        assert!(tool.parameters().get("required").is_none());
    }
}
