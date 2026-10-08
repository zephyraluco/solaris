//! `find` — locate paths by glob.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::path_utils;
use crate::runner::CommandRunner;
use crate::tool::{
    Tool, ToolContext, ToolOutput, optional_count_arg, optional_string_arg, string_arg,
};
use crate::truncate::{Limits, head};

/// Most paths one search returns.
const DEFAULT_LIMIT: usize = 1000;

/// Finds paths with fd.
#[derive(Clone)]
pub struct FindTool {
    runner: Arc<dyn CommandRunner>,
    limits: Limits,
    default_limit: usize,
}

impl FindTool {
    /// A finder that shells out to `fd`.
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self {
            runner,
            limits: Limits::default(),
            default_limit: DEFAULT_LIMIT,
        }
    }

    /// A finder with its own output budget.
    pub fn with_limits(mut self, limits: Limits, default_limit: usize) -> Self {
        self.limits = limits;
        self.default_limit = default_limit;
        self
    }
}

#[async_trait]
impl Tool for FindTool {
    fn name(&self) -> &str {
        "find"
    }

    fn description(&self) -> &str {
        "Find files and directories by glob pattern, returning paths. Files ignored by .gitignore \
         are skipped, and dotfiles are included. Use `grep` to search what is inside files."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob to match, e.g. '*.rs', '**/*.test.ts' or 'src/**'",
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search in (default: the session directory)",
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of paths to return (default: 1000)",
                },
            },
            "required": ["pattern"],
        })
    }

    async fn run(&self, input: Value, ctx: &ToolContext) -> ToolOutput {
        let pattern = match string_arg(&input, "pattern") {
            Ok(pattern) => pattern,
            Err(error) => return error,
        };
        let search_path = match optional_string_arg(&input, "path") {
            Ok(path) => path.unwrap_or_else(|| ".".to_string()),
            Err(error) => return error,
        };
        let limit = match optional_count_arg(&input, "limit") {
            Ok(limit) => limit.unwrap_or(self.default_limit),
            Err(error) => return error,
        };

        let absolute = path_utils::resolve(&ctx.cwd, &search_path);
        if !path_utils::exists(&absolute).await {
            return ToolOutput::error(format!("{search_path} does not exist"));
        }

        let args = fd_args(&pattern, &search_path);
        let output = match self.runner.run("fd", &args, &ctx.cwd).await {
            Ok(output) => output,
            Err(error) => return ToolOutput::error(error),
        };
        if !output.succeeded() {
            return ToolOutput::error(format!("fd failed: {}", output.failure_message()));
        }

        let mut paths: Vec<String> = output
            .stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect();
        paths.sort_by_key(|path| path.to_lowercase());

        if paths.is_empty() {
            return ToolOutput::ok(format!("no paths match `{pattern}` in {search_path}"));
        }

        let mut notices = Vec::new();
        if paths.len() > limit {
            notices.push(format!("stopped at {limit} paths; raise `limit` for more"));
            paths.truncate(limit);
        }

        let listing = paths.join("\n");
        let truncation = head(&listing, self.limits);
        if let Some(notice) = truncation.notice(self.limits) {
            notices.push(notice);
        }

        let mut text = truncation.content;
        if !notices.is_empty() {
            text.push_str(&format!("\n\n[{}]", notices.join(". ")));
        }
        ToolOutput::ok(text)
    }
}

/// The fd invocation for one search.
///
/// `--no-require-git` is always passed so a `.gitignore` is honoured in a plain
/// directory too: a person who wrote one there meant it.
fn fd_args(pattern: &str, search_path: &str) -> Vec<String> {
    let mut args = vec![
        "--color=never".to_string(),
        "--hidden".to_string(),
        "--no-require-git".to_string(),
        "--glob".to_string(),
    ];
    // fd matches a glob against the file name unless told to consider the whole
    // path, which is what a pattern containing `/` is asking for.
    if pattern.contains('/') {
        args.push("--full-path".to_string());
    }
    args.push(pattern.to_string());
    args.push(search_path.to_string());
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::CommandOutput;
    use crate::runner::tests::StubRunner;
    use crate::tools::test_dir;
    use std::path::Path;

    fn tool(paths: &[&str]) -> FindTool {
        let stdout = paths.join("\n");
        FindTool::new(Arc::new(StubRunner(CommandOutput {
            stdout,
            code: Some(0),
            ..Default::default()
        })))
    }

    fn context(dir: &Path) -> ToolContext {
        ToolContext::new(dir, dir)
    }

    #[test]
    fn a_plain_glob_says_nothing_about_paths() {
        assert_eq!(
            fd_args("*.rs", "."),
            vec![
                "--color=never",
                "--hidden",
                "--no-require-git",
                "--glob",
                "*.rs",
                ".",
            ]
        );
    }

    #[test]
    fn a_glob_with_a_separator_matches_whole_paths() {
        let args = fd_args("src/**/*.rs", ".");
        assert!(args.contains(&"--full-path".to_string()));
    }

    #[tokio::test]
    async fn paths_come_back_sorted() {
        let dir = test_dir("find-sorted");
        let result = tool(&["src/b.rs", "src/a.rs", "Cargo.toml"])
            .run(json!({ "pattern": "*.rs" }), &context(&dir))
            .await;

        assert!(!result.is_error, "{}", result.text);
        assert_eq!(result.text, "Cargo.toml\nsrc/a.rs\nsrc/b.rs");
    }

    #[tokio::test]
    async fn nothing_found_is_an_answer_not_a_failure() {
        let dir = test_dir("find-none");
        let result = tool(&[])
            .run(json!({ "pattern": "*.zzz" }), &context(&dir))
            .await;

        assert!(!result.is_error, "{}", result.text);
        assert!(result.text.contains("no paths match"), "{}", result.text);
    }

    #[tokio::test]
    async fn the_limit_is_reported_with_a_way_past_it() {
        let dir = test_dir("find-limit");
        let result = tool(&["a", "b", "c"])
            .run(json!({ "pattern": "*", "limit": 2 }), &context(&dir))
            .await;

        assert!(result.text.starts_with("a\nb\n"), "{}", result.text);
        assert!(!result.text.contains("\nc\n"), "{}", result.text);
        assert!(result.text.contains("raise `limit`"), "{}", result.text);
    }

    #[tokio::test]
    async fn a_failure_is_reported_with_fds_own_wording() {
        let dir = test_dir("find-failure");
        let tool = FindTool::new(Arc::new(StubRunner(CommandOutput {
            stderr: "invalid glob".to_string(),
            code: Some(1),
            ..Default::default()
        })));

        let result = tool.run(json!({ "pattern": "[" }), &context(&dir)).await;

        assert!(result.is_error);
        assert!(result.text.contains("invalid glob"), "{}", result.text);
    }

    #[tokio::test]
    async fn searching_somewhere_that_is_not_there_is_refused_before_running_fd() {
        let dir = test_dir("find-missing-path");
        let result = tool(&[])
            .run(json!({ "pattern": "*", "path": "nope" }), &context(&dir))
            .await;

        assert!(result.is_error);
        assert!(result.text.contains("does not exist"), "{}", result.text);
    }

    #[test]
    fn the_declaration_requires_a_pattern() {
        let tool = tool(&[]);
        assert_eq!(tool.name(), "find");
        assert_eq!(tool.parameters()["required"][0], "pattern");
    }
}
