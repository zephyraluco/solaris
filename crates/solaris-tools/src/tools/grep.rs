//! `grep` — search file contents.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::path_utils;
use crate::runner::CommandRunner;
use crate::tool::{
    Tool, ToolContext, ToolOutput, optional_bool_arg, optional_count_arg, optional_string_arg,
    string_arg,
};
use crate::truncate::{Limits, head};

/// Most matches one search returns.
const DEFAULT_LIMIT: usize = 100;

/// Searches file contents with ripgrep.
#[derive(Clone)]
pub struct GrepTool {
    runner: Arc<dyn CommandRunner>,
    limits: Limits,
    default_limit: usize,
}

impl GrepTool {
    /// A searcher that shells out to `rg`.
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self {
            runner,
            limits: Limits::default(),
            default_limit: DEFAULT_LIMIT,
        }
    }

    /// A searcher with its own output budget.
    pub fn with_limits(mut self, limits: Limits, default_limit: usize) -> Self {
        self.limits = limits;
        self.default_limit = default_limit;
        self
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents with ripgrep and return matching lines as `path:line: text`. Files \
         ignored by .gitignore are skipped. Use `glob` to narrow the files searched, and `literal` \
         when the pattern contains regex characters that should be matched as they are."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regular expression to search for, or literal text with `literal`",
                },
                "path": {
                    "type": "string",
                    "description": "Directory or file to search (default: the session directory)",
                },
                "glob": {
                    "type": "string",
                    "description": "Only search files matching this glob, e.g. '*.rs' or 'src/**'",
                },
                "ignoreCase": {
                    "type": "boolean",
                    "description": "Match case-insensitively",
                },
                "literal": {
                    "type": "boolean",
                    "description": "Treat the pattern as literal text rather than a regex",
                },
                "context": {
                    "type": "integer",
                    "description": "Lines of context to show around each match",
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of matches to return (default: 100)",
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
        let glob = match optional_string_arg(&input, "glob") {
            Ok(glob) => glob,
            Err(error) => return error,
        };
        let ignore_case = match optional_bool_arg(&input, "ignoreCase") {
            Ok(flag) => flag.unwrap_or(false),
            Err(error) => return error,
        };
        let literal = match optional_bool_arg(&input, "literal") {
            Ok(flag) => flag.unwrap_or(false),
            Err(error) => return error,
        };
        let context = match optional_count_arg(&input, "context") {
            Ok(context) => context.unwrap_or(0),
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

        let args = ripgrep_args(&pattern, &search_path, glob.as_deref(), ignore_case, literal, context);
        let output = match self.runner.run("rg", &args, &ctx.cwd).await {
            Ok(output) => output,
            Err(error) => return ToolOutput::error(error),
        };

        // Ripgrep exits 1 when nothing matched, which is an answer rather than a
        // failure; anything else is a real problem, usually a bad pattern.
        if !output.succeeded() && output.code != Some(1) {
            return ToolOutput::error(format!("ripgrep failed: {}", output.failure_message()));
        }

        let (matches, capped) = parse_ripgrep(&output.stdout, limit);
        if matches.is_empty() {
            return ToolOutput::ok(format!("no matches for `{pattern}` in {search_path}"));
        }

        let listing = matches.join("\n");
        let truncation = head(&listing, self.limits);

        let mut notices = Vec::new();
        if capped {
            notices.push(format!("stopped at {limit} matches; raise `limit` for more"));
        }
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

/// The ripgrep invocation for one search.
///
/// `--hidden` includes dotfiles but leaves `.gitignore` in force, which is what
/// a person means by "search the project".
fn ripgrep_args(
    pattern: &str,
    search_path: &str,
    glob: Option<&str>,
    ignore_case: bool,
    literal: bool,
    context: usize,
) -> Vec<String> {
    let mut args = vec![
        "--json".to_string(),
        "--line-number".to_string(),
        "--color=never".to_string(),
        "--hidden".to_string(),
    ];
    if ignore_case {
        args.push("--ignore-case".to_string());
    }
    if literal {
        args.push("--fixed-strings".to_string());
    }
    if let Some(glob) = glob {
        args.push("--glob".to_string());
        args.push(glob.to_string());
    }
    if context > 0 {
        args.push("--context".to_string());
        args.push(context.to_string());
    }
    // `--` so a pattern that starts with `-` is still a pattern.
    args.push("--".to_string());
    args.push(pattern.to_string());
    args.push(search_path.to_string());
    args
}

/// Turn ripgrep's JSON events into `path:line: text` lines.
///
/// Only `match` events count towards `limit`; context lines ride along with the
/// match they belong to. The flag says whether the limit cut the search short.
fn parse_ripgrep(stdout: &str, limit: usize) -> (Vec<String>, bool) {
    let mut lines = Vec::new();
    let mut matches = 0usize;

    for line in stdout.lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let separator = match event["type"].as_str() {
            Some("match") => {
                if matches >= limit {
                    return (lines, true);
                }
                matches += 1;
                ':'
            }
            Some("context") => '-',
            // `begin`, `end` and `summary` carry no line of their own.
            _ => continue,
        };

        let data = &event["data"];
        let Some(path) = data["path"]["text"].as_str() else {
            continue;
        };
        let number = data["line_number"].as_u64().unwrap_or(0);
        let text = data["lines"]["text"]
            .as_str()
            .unwrap_or_default()
            .trim_end_matches('\n');
        lines.push(format!("{path}{separator}{number}{separator}{text}"));
    }

    (lines, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::tests::StubRunner;
    use crate::runner::CommandOutput;
    use crate::tools::test_dir;
    use std::path::Path;

    fn tool(output: CommandOutput) -> GrepTool {
        GrepTool::new(Arc::new(StubRunner(output)))
    }

    /// A ripgrep that succeeded, with `stdout`.
    fn ok(stdout: String) -> CommandOutput {
        CommandOutput {
            stdout,
            code: Some(0),
            ..Default::default()
        }
    }

    fn context(dir: &Path) -> ToolContext {
        ToolContext::new(dir, dir)
    }

    fn match_event(path: &str, line: u64, text: &str) -> String {
        json!({
            "type": "match",
            "data": {
                "path": { "text": path },
                "line_number": line,
                "lines": { "text": format!("{text}\n") },
            },
        })
        .to_string()
    }

    #[test]
    fn the_invocation_is_what_ripgrep_expects() {
        let args = ripgrep_args("fn main", "src", Some("*.rs"), true, true, 2);
        assert_eq!(
            args,
            vec![
                "--json",
                "--line-number",
                "--color=never",
                "--hidden",
                "--ignore-case",
                "--fixed-strings",
                "--glob",
                "*.rs",
                "--context",
                "2",
                "--",
                "fn main",
                "src",
            ]
        );
    }

    #[test]
    fn a_pattern_that_looks_like_a_flag_is_still_a_pattern() {
        let args = ripgrep_args("--version", ".", None, false, false, 0);
        let separator = args.iter().position(|arg| arg == "--").expect("--");
        assert_eq!(args[separator + 1], "--version");
    }

    #[test]
    fn matches_and_context_are_rendered_the_way_grep_does() {
        let stdout = format!(
            "{}\n{}\n{}\n",
            match_event("src/a.rs", 3, "fn main() {"),
            json!({
                "type": "context",
                "data": {
                    "path": { "text": "src/a.rs" },
                    "line_number": 4,
                    "lines": { "text": "    body\n" },
                },
            }),
            match_event("src/b.rs", 9, "fn main() {"),
        );
        let (lines, capped) = parse_ripgrep(&stdout, 10);

        assert_eq!(
            lines,
            vec![
                "src/a.rs:3:fn main() {",
                "src/a.rs-4-    body",
                "src/b.rs:9:fn main() {",
            ]
        );
        assert!(!capped);
    }

    #[test]
    fn the_limit_counts_matches_and_reports_that_it_cut() {
        let stdout = (1..=5)
            .map(|line| match_event("a.rs", line, "hit"))
            .collect::<Vec<_>>()
            .join("\n");
        let (lines, capped) = parse_ripgrep(&stdout, 2);

        assert_eq!(lines.len(), 2);
        assert!(capped);
    }

    #[test]
    fn noise_ripgrep_writes_is_skipped() {
        let stdout = format!(
            "{}\nnot json\n{}\n",
            json!({ "type": "begin", "data": { "path": { "text": "a.rs" } } }),
            json!({ "type": "summary", "data": { "stats": {} } }),
        );
        let (lines, capped) = parse_ripgrep(&stdout, 10);
        assert!(lines.is_empty());
        assert!(!capped);
    }

    #[tokio::test]
    async fn a_match_comes_back_as_a_line() {
        let dir = test_dir("grep-match");
        let output = ok(match_event("src/a.rs", 3, "fn main() {"));

        let result = tool(output)
            .run(json!({ "pattern": "fn main" }), &context(&dir))
            .await;

        assert!(!result.is_error, "{}", result.text);
        assert_eq!(result.text, "src/a.rs:3:fn main() {");
    }

    #[tokio::test]
    async fn nothing_matched_is_an_answer_not_a_failure() {
        let dir = test_dir("grep-none");
        let output = CommandOutput {
            code: Some(1),
            ..Default::default()
        };

        let result = tool(output)
            .run(json!({ "pattern": "nothing" }), &context(&dir))
            .await;

        assert!(!result.is_error, "{}", result.text);
        assert!(result.text.contains("no matches"), "{}", result.text);
    }

    #[tokio::test]
    async fn a_bad_pattern_is_reported_with_ripgreps_own_wording() {
        let dir = test_dir("grep-bad");
        let output = CommandOutput {
            stderr: "regex parse error".to_string(),
            code: Some(2),
            ..Default::default()
        };

        let result = tool(output)
            .run(json!({ "pattern": "(" }), &context(&dir))
            .await;

        assert!(result.is_error);
        assert!(result.text.contains("regex parse error"), "{}", result.text);
    }

    #[tokio::test]
    async fn a_missing_ripgrep_explains_how_to_get_it() {
        struct Missing;
        #[async_trait]
        impl CommandRunner for Missing {
            async fn run(
                &self,
                program: &str,
                _args: &[String],
                _cwd: &Path,
            ) -> Result<CommandOutput, String> {
                Err(format!("`{program}` was not found on PATH — install it and try again"))
            }
        }

        let dir = test_dir("grep-missing-rg");
        let result = GrepTool::new(Arc::new(Missing))
            .run(json!({ "pattern": "x" }), &context(&dir))
            .await;

        assert!(result.is_error);
        assert!(result.text.contains("PATH"), "{}", result.text);
    }

    #[tokio::test]
    async fn searching_somewhere_that_is_not_there_is_refused_before_running_rg() {
        let dir = test_dir("grep-missing-path");
        let result = tool(CommandOutput::default())
            .run(json!({ "pattern": "x", "path": "nope" }), &context(&dir))
            .await;

        assert!(result.is_error);
        assert!(result.text.contains("does not exist"), "{}", result.text);
    }

    #[test]
    fn the_declaration_requires_a_pattern() {
        let tool = tool(CommandOutput::default());
        assert_eq!(tool.name(), "grep");
        assert_eq!(tool.parameters()["required"][0], "pattern");
    }
}
