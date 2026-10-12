//! The mode a session runs in, and the tools each mode declares.

use serde::{Deserialize, Serialize};
use solaris_tools::SHELL_TOOL;

/// Agent operating mode; decides which tools a turn declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Build,
    Plan,
}

impl Mode {
    /// Uppercase badge label shown in the status bar.
    pub fn label(self) -> &'static str {
        match self {
            Mode::Build => "BUILD",
            Mode::Plan => "PLAN",
        }
    }

    /// Cycle to the other mode (Tab).
    pub fn next(self) -> Self {
        match self {
            Mode::Build => Mode::Plan,
            Mode::Plan => Mode::Build,
        }
    }
}

/// The tools Build mode declares: it runs commands and changes files.
const BUILD_TOOLS: &[&str] = &["read", SHELL_TOOL, "edit", "write"];

/// The tools Plan mode declares: it only looks.
const PLAN_TOOLS: &[&str] = &["read", "grep", "find", "ls"];

/// The tools a mode declares when nothing else is configured.
///
/// Plan mode withdraws the tools that write before the model ever sees them:
/// it cannot be talked into changing a file by a prompt, because there is no
/// tool to call.
pub fn default_tool_names(mode: Mode) -> &'static [&'static str] {
    match mode {
        Mode::Build => BUILD_TOOLS,
        Mode::Plan => PLAN_TOOLS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_mode_runs_and_edits_while_plan_mode_only_looks() {
        let build = default_tool_names(Mode::Build);
        assert!(build.contains(&"read"));
        assert!(build.contains(&SHELL_TOOL));
        assert!(build.contains(&"edit"));
        assert!(build.contains(&"write"));
        assert!(!build.contains(&"grep"), "not in the default set");

        let plan = default_tool_names(Mode::Plan);
        assert_eq!(plan, ["read", "grep", "find", "ls"]);
        assert!(!plan.contains(&SHELL_TOOL));
        assert!(!plan.contains(&"write"));
    }

    #[test]
    fn the_mode_cycles_between_the_two_labels() {
        assert_eq!(Mode::default(), Mode::Build);
        assert_eq!(Mode::Build.next(), Mode::Plan);
        assert_eq!(Mode::Plan.next(), Mode::Build);
        assert_eq!(Mode::Plan.label(), "PLAN");
    }
}
