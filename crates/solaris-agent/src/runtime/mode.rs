//! The mode a session runs in, and the tools each mode declares.

use serde::{Deserialize, Serialize};
use solaris_tools::SHELL_TOOL;

/// Agent operating mode; decides which tools a turn declares and how tool
/// approvals behave.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    #[serde(alias = "build")]
    Default,
    Plan,
    Auto,
    Ask,
}

impl Mode {
    /// All modes in Tab cycling order.
    pub const ALL: [Mode; 4] = [Mode::Default, Mode::Plan, Mode::Auto, Mode::Ask];

    /// Stable index matching [`Mode::ALL`].
    pub(crate) const fn index(self) -> usize {
        match self {
            Mode::Default => 0,
            Mode::Plan => 1,
            Mode::Auto => 2,
            Mode::Ask => 3,
        }
    }

    /// Canonical lowercase identifier (`default`, `plan`, `auto`, `ask`).
    pub fn id(self) -> &'static str {
        match self {
            Mode::Default => "default",
            Mode::Plan => "plan",
            Mode::Auto => "auto",
            Mode::Ask => "ask",
        }
    }

    /// Parse a mode name case-insensitively.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "default" => Some(Mode::Default),
            "plan" => Some(Mode::Plan),
            "auto" => Some(Mode::Auto),
            "ask" => Some(Mode::Ask),
            _ => None,
        }
    }

    /// Uppercase badge label shown in the status bar.
    pub fn label(self) -> &'static str {
        match self {
            Mode::Default => "DEFAULT",
            Mode::Plan => "PLAN",
            Mode::Auto => "AUTO",
            Mode::Ask => "ASK",
        }
    }

    /// Whether this mode restricts the session to read-only tools.
    pub const fn is_read_only(self) -> bool {
        matches!(self, Mode::Plan)
    }

    /// Cycle to the next mode (Tab): `default -> plan -> auto -> ask -> default`.
    pub fn next(self) -> Self {
        match self {
            Mode::Default => Mode::Plan,
            Mode::Plan => Mode::Auto,
            Mode::Auto => Mode::Ask,
            Mode::Ask => Mode::Default,
        }
    }

    /// What the mode means for this turn, as the system prompt says it.
    ///
    /// It lives beside the mode because it is the same fact as the tool set: a
    /// mode that withdraws `write` and `edit` has to *say* so, or the model reads
    /// a missing tool as a broken one and starts describing the edit it cannot
    /// make.
    pub fn instructions(self) -> &'static str {
        match self {
            Mode::Default => {
                "You are in default mode: you may read and search, run commands, and change files. \
                 Read-only tools run automatically; file changes and commands require user \
                 confirmation."
            }
            Mode::Plan => {
                "You are in plan mode: you may only read, search and list. Withdrawn for this \
                 turn are the tools that change files or run commands, so work out what you would \
                 do and propose it in your reply instead of trying to make the change."
            }
            Mode::Auto => {
                "You are in auto mode: you may read and search, run commands, and change files \
                 autonomously; tool calls run automatically without confirmation."
            }
            Mode::Ask => {
                "You are in ask mode: you may read and search, run commands, and change files, \
                 but every tool call — including read-only inspection — requires user confirmation \
                 before it runs."
            }
        }
    }
}

/// The tools full-capability modes (`default`, `auto`, `ask`) declare.
const DEFAULT_TOOLS: &[&str] = &["read", SHELL_TOOL, "edit", "write", "subagent"];

/// The tools Plan mode declares: it only looks.
const PLAN_TOOLS: &[&str] = &["read", "grep", "find", "ls", "subagent"];

/// The tools a mode declares when nothing else is configured.
///
/// Plan mode withdraws the tools that write before the model ever sees them:
/// it cannot be talked into changing a file by a prompt, because there is no
/// tool to call.
pub fn default_tool_names(mode: Mode) -> &'static [&'static str] {
    match mode {
        Mode::Default | Mode::Auto | Mode::Ask => DEFAULT_TOOLS,
        Mode::Plan => PLAN_TOOLS,
    }
}

/// Resolve common defaults and extension discovery before caller-specific restrictions.
pub(crate) fn select_tools(
    registry: &solaris_tools::ToolRegistry,
    mode: Mode,
    selection: &solaris_tools::ToolSelection,
) -> Vec<std::sync::Arc<dyn solaris_tools::Tool>> {
    let mut defaults = default_tool_names(mode).to_vec();
    let external: Vec<String> = registry
        .names()
        .into_iter()
        .filter(|name| {
            name.starts_with("mcp_")
                && registry
                    .get(name)
                    .is_some_and(|tool| !mode.is_read_only() || tool.is_read_only())
        })
        .collect();
    defaults.extend(external.iter().map(String::as_str));
    if registry.get("question").is_some() {
        defaults.push("question");
    }
    if registry.get("tool_search").is_some() {
        defaults.push("tool_search");
    }
    registry.select(&defaults, selection)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_modes_run_and_edit_while_plan_mode_only_looks() {
        for mode in [Mode::Default, Mode::Auto, Mode::Ask] {
            let tools = default_tool_names(mode);
            assert!(tools.contains(&"read"));
            assert!(tools.contains(&SHELL_TOOL));
            assert!(tools.contains(&"edit"));
            assert!(tools.contains(&"write"));
            assert!(!tools.contains(&"grep"), "not in the default set");
        }

        let plan = default_tool_names(Mode::Plan);
        assert_eq!(plan, ["read", "grep", "find", "ls", "subagent"]);
        assert!(!plan.contains(&SHELL_TOOL));
        assert!(!plan.contains(&"write"));
    }

    #[test]
    fn modes_cycle_and_parse_across_all_four_variants() {
        assert_eq!(Mode::default(), Mode::Default);
        assert_eq!(Mode::Default.next(), Mode::Plan);
        assert_eq!(Mode::Plan.next(), Mode::Auto);
        assert_eq!(Mode::Auto.next(), Mode::Ask);
        assert_eq!(Mode::Ask.next(), Mode::Default);

        assert_eq!(Mode::Default.label(), "DEFAULT");
        assert_eq!(Mode::Plan.label(), "PLAN");
        assert_eq!(Mode::Auto.label(), "AUTO");
        assert_eq!(Mode::Ask.label(), "ASK");

        assert_eq!(Mode::parse("default"), Some(Mode::Default));
        assert_eq!(Mode::parse("PLAN"), Some(Mode::Plan));
        assert_eq!(Mode::parse("auto"), Some(Mode::Auto));
        assert_eq!(Mode::parse("ask"), Some(Mode::Ask));
        assert_eq!(Mode::parse("unknown"), None);

        let legacy: Mode = serde_json::from_str("\"build\"").expect("legacy alias");
        assert_eq!(legacy, Mode::Default);
    }
}
