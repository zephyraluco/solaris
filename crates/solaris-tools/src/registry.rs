//! Which tools a session has, and which of them a turn declares.
//!
//! The registry holds every built-in tool this platform can run. Which of them a
//! turn actually offers is decided per request: the caller names a default set
//! and a selection edits it, so withdrawing the tools that write is a matter of
//! handing over a different default set — which is how Plan mode keeps the model
//! from writing before it ever sees a file.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::mutation_queue::FileMutationQueue;
use crate::runner::{CommandRunner, LocalRunner};
use crate::tool::Tool;
use crate::tools::edit::EditTool;
use crate::tools::find::FindTool;
use crate::tools::grep::GrepTool;
use crate::tools::ls::LsTool;
use crate::tools::read::ReadTool;
use crate::tools::shell::{LocalShell, ShellRunner, ShellTool};
use crate::tools::write::WriteTool;

/// Every tool this build implements, by name.
///
/// Both shells are listed because both are implemented, but a registry only
/// ever holds the one its platform has: offering `bash` on Windows would be
/// offering something that cannot run.
pub const BUILTIN_TOOLS: &[&str] = &[
    "read",
    "bash",
    "powershell",
    "edit",
    "write",
    "grep",
    "find",
    "ls",
];

/// The shell tool this platform has.
pub const SHELL_TOOL: &str = if cfg!(windows) { "powershell" } else { "bash" };

/// The tools a session can declare.
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// A registry whose external programs are the real ones.
    pub fn builtin() -> Self {
        Self::with_runners(Arc::new(LocalRunner), Arc::new(LocalShell))
    }

    /// A registry that reaches ripgrep, fd and the shell through `commands` and
    /// `shell`.
    ///
    /// The seam a test stands in: the suite runs the whole tool set without
    /// either search program installed and without spawning a shell.
    pub fn with_runners(commands: Arc<dyn CommandRunner>, shell: Arc<dyn ShellRunner>) -> Self {
        let queue = Arc::new(FileMutationQueue::new());
        let mut registry = Self {
            tools: BTreeMap::new(),
        };

        registry.add(Arc::new(ReadTool::new()));
        registry.add(Arc::new(EditTool::new(Arc::clone(&queue))));
        registry.add(Arc::new(WriteTool::new(queue)));
        registry.add(Arc::new(LsTool::new()));
        registry.add(Arc::new(GrepTool::new(Arc::clone(&commands))));
        registry.add(Arc::new(FindTool::new(commands)));

        let shell_tool: Arc<dyn Tool> = match SHELL_TOOL {
            "powershell" => Arc::new(ShellTool::powershell(shell)),
            _ => Arc::new(ShellTool::bash(shell)),
        };
        registry.add(shell_tool);

        registry
    }

    /// A registry holding exactly `tools`, for a caller with its own set.
    pub fn with_tools(tools: Vec<Arc<dyn Tool>>) -> Self {
        let mut registry = Self {
            tools: BTreeMap::new(),
        };
        for tool in tools {
            registry.add(tool);
        }
        registry
    }

    /// Put `tool` in, under the name it answers to.
    fn add(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    /// The tool called `name`, when this platform has it.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// Every tool in the registry, alphabetically.
    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// How many tools the registry holds.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether it holds none.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// The tools to declare for `defaults` under `selection`, in a stable order.
    ///
    /// The default set is the caller's: this crate knows which tools it has, not
    /// which of them a turn should offer. Names in `defaults` this platform does
    /// not have are simply not declared.
    pub fn select(&self, defaults: &[&str], selection: &ToolSelection) -> Vec<Arc<dyn Tool>> {
        self.selected_names(defaults, selection)
            .into_iter()
            .filter_map(|name| self.get(&name))
            .collect()
    }

    /// The names [`ToolRegistry::select`] would resolve, for callers that want
    /// to show them before running anything.
    pub fn selected_names(&self, defaults: &[&str], selection: &ToolSelection) -> Vec<String> {
        let mut names = if selection.only.is_empty() {
            defaults.iter().map(|name| (*name).to_string()).collect()
        } else {
            self.expand(&selection.only)
        };

        for name in self.expand(&selection.add) {
            if !names.contains(&name) {
                names.push(name);
            }
        }

        names.retain(|name| {
            self.tools.contains_key(name.as_str()) && !matches_any(&selection.remove, name)
        });
        names.sort();
        names.dedup();
        names
    }

    /// Names or patterns in `selection` that match no tool here.
    ///
    /// A non-empty answer means the user asked for something this build does not
    /// have — a typo worth saying out loud rather than silently ignoring.
    pub fn unmatched(&self, selection: &ToolSelection) -> Vec<String> {
        selection
            .entries()
            .into_iter()
            .filter(|entry| !self.tools.keys().any(|name| matches_pattern(entry, name)))
            .collect()
    }

    /// Expand names and `*` patterns against what is actually here.
    fn expand(&self, entries: &[String]) -> Vec<String> {
        self.tools
            .keys()
            .filter(|name| matches_any(entries, name))
            .cloned()
            .collect()
    }
}

/// Which tools a session declares.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolSelection {
    /// An explicit allowlist, which replaces the default set entirely.
    pub only: Vec<String>,
    /// Names or `*` patterns added to the default set.
    pub add: Vec<String>,
    /// Names or patterns removed after everything else.
    pub remove: Vec<String>,
}

impl ToolSelection {
    /// The default selection: whatever the caller's default set declares.
    pub fn none() -> Self {
        Self::default()
    }

    /// Read a `--tools` list and an `--exclude-tools` list.
    ///
    /// `--tools` is an allowlist, unless every entry is a `+name`/`-name`
    /// adjustment — then it edits the default set instead, which is how one tool
    /// gets added without naming the rest. Mixing the two forms is refused
    /// rather than guessed at.
    pub fn parse(tools: Option<&str>, exclude: Option<&str>) -> Result<Self, String> {
        let mut selection = Self::default();

        if let Some(list) = tools {
            let entries = split_list(list);
            let adjustments = entries
                .iter()
                .filter(|entry| entry.starts_with('+') || entry.starts_with('-'))
                .count();

            if adjustments == entries.len() {
                for entry in entries {
                    if let Some(name) = entry.strip_prefix('+') {
                        selection.add.push(name.to_string());
                    } else if let Some(name) = entry.strip_prefix('-') {
                        selection.remove.push(name.to_string());
                    }
                }
            } else if adjustments > 0 {
                return Err(
                    "`--tools` cannot mix plain names with `+name`/`-name` entries: name every \
                     tool you want in the allowlist, or adjust the default set"
                        .to_string(),
                );
            } else {
                selection.only = entries;
            }
        }

        if let Some(list) = exclude {
            selection.remove.extend(split_list(list));
        }

        Ok(selection)
    }

    /// Every entry, for a caller that wants to check them all.
    pub fn entries(&self) -> Vec<String> {
        let mut entries = self.only.clone();
        entries.extend(self.add.iter().cloned());
        entries.extend(self.remove.iter().cloned());
        entries
    }

    /// Whether this selection changes anything.
    pub fn is_default(&self) -> bool {
        self.only.is_empty() && self.add.is_empty() && self.remove.is_empty()
    }
}

/// Split a comma-separated list, dropping blanks.
fn split_list(list: &str) -> Vec<String> {
    list.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_string)
        .collect()
}

/// Whether `name` matches any of `entries`.
fn matches_any(entries: &[String], name: &str) -> bool {
    entries.iter().any(|entry| matches_pattern(entry, name))
}

/// Whether `name` matches `pattern`, where `*` stands for any run of characters.
fn matches_pattern(pattern: &str, name: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == name;
    }

    let mut segments = pattern.split('*');
    let Some(first) = segments.next() else {
        return false;
    };
    let Some(mut rest) = name.strip_prefix(first) else {
        return false;
    };

    for segment in segments {
        if segment.is_empty() {
            continue;
        }
        match rest.find(segment) {
            Some(index) => rest = &rest[index + segment.len()..],
            None => return false,
        }
    }

    // A pattern that does not end in `*` has to have consumed the whole name.
    pattern.ends_with('*') || rest.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::CommandOutput;
    use crate::runner::tests::StubRunner;
    use crate::tools::shell::{ShellConfig, ShellOutcome};

    struct StubShell;

    #[async_trait::async_trait]
    impl ShellRunner for StubShell {
        async fn run(
            &self,
            _config: ShellConfig,
            _command: &str,
            _ctx: &crate::tool::ToolContext,
            _timeout: Option<std::time::Duration>,
        ) -> Result<ShellOutcome, String> {
            Ok(ShellOutcome::default())
        }
    }

    /// The default set the tests declare: the same shape a session starts from.
    const DEFAULTS: &[&str] = &["read", SHELL_TOOL, "edit", "write"];

    fn registry() -> ToolRegistry {
        ToolRegistry::with_runners(
            Arc::new(StubRunner(CommandOutput::default())),
            Arc::new(StubShell),
        )
    }

    fn names(tools: &[Arc<dyn Tool>]) -> Vec<String> {
        tools.iter().map(|tool| tool.name().to_string()).collect()
    }

    #[test]
    fn the_registry_holds_one_shell_for_this_platform() {
        let registry = registry();
        assert!(registry.get(SHELL_TOOL).is_some());
        let other = if SHELL_TOOL == "bash" {
            "powershell"
        } else {
            "bash"
        };
        assert!(registry.get(other).is_none(), "only one shell can run here");
        assert_eq!(registry.get("read").expect("read").name(), "read");
    }

    #[test]
    fn the_default_set_is_what_a_plain_selection_declares() {
        let registry = registry();

        let declared = names(&registry.select(DEFAULTS, &ToolSelection::none()));
        assert!(declared.contains(&"read".to_string()));
        assert!(declared.contains(&SHELL_TOOL.to_string()));
        assert!(declared.contains(&"edit".to_string()));
        assert!(declared.contains(&"write".to_string()));
        assert!(
            !declared.contains(&"grep".to_string()),
            "not in the default set"
        );

        // A name the default set mentions but this platform lacks is dropped
        // rather than declared as something that could not run.
        let declared = names(&registry.select(&["read", "teleport"], &ToolSelection::none()));
        assert_eq!(declared, vec!["read"]);
    }

    #[test]
    fn an_allowlist_replaces_the_default_set() {
        let registry = registry();
        let selection = ToolSelection {
            only: vec!["read".to_string(), "grep".to_string()],
            ..Default::default()
        };

        assert_eq!(
            names(&registry.select(DEFAULTS, &selection)),
            vec!["grep", "read"]
        );
    }

    #[test]
    fn additions_and_removals_edit_the_default_set() {
        let registry = registry();
        let added = ToolSelection {
            add: vec!["grep".to_string()],
            ..Default::default()
        };
        let selected = names(&registry.select(DEFAULTS, &added));
        assert!(selected.contains(&"grep".to_string()));
        assert!(selected.contains(&"read".to_string()), "the rest is kept");

        let removed = ToolSelection {
            remove: vec!["write".to_string()],
            ..Default::default()
        };
        let selected = names(&registry.select(DEFAULTS, &removed));
        assert!(!selected.contains(&"write".to_string()));
        assert!(selected.contains(&"edit".to_string()));
    }

    #[test]
    fn a_star_pattern_selects_a_family_of_tools() {
        let registry = registry();
        // `*d` reaches the two tools whose names end in `d`, whichever shell
        // this platform has.
        let selection = ToolSelection {
            only: vec!["*d".to_string()],
            ..Default::default()
        };
        assert_eq!(
            names(&registry.select(DEFAULTS, &selection)),
            vec!["find", "read"]
        );

        let selection = ToolSelection {
            only: vec!["gr*".to_string()],
            ..Default::default()
        };
        assert_eq!(names(&registry.select(DEFAULTS, &selection)), vec!["grep"]);
    }

    #[test]
    fn a_pattern_does_not_match_a_name_it_should_not() {
        assert!(matches_pattern("read", "read"));
        assert!(!matches_pattern("read", "reader"));
        assert!(!matches_pattern("read", "bread"));
        assert!(matches_pattern("read*", "reader"));
        assert!(matches_pattern("*read", "bread"));
        assert!(matches_pattern("*", "anything"));
        assert!(matches_pattern("mcp__*", "mcp__radius"));
        assert!(!matches_pattern("mcp__*", "radius"));
        assert!(matches_pattern("mcp__*__x", "mcp__radius__x"));
        assert!(!matches_pattern("mcp__*__x", "mcp__radius__y"));
    }

    #[test]
    fn a_selection_is_parsed_the_way_the_command_line_reads_it() {
        let selection = ToolSelection::parse(Some("read,grep"), None).expect("parsed");
        assert_eq!(selection.only, vec!["read", "grep"]);
        assert!(selection.add.is_empty());

        let selection = ToolSelection::parse(Some("+grep,-write"), Some("find")).expect("parsed");
        assert!(selection.only.is_empty());
        assert_eq!(selection.add, vec!["grep"]);
        assert_eq!(selection.remove, vec!["write", "find"]);

        let selection = ToolSelection::parse(Some(" +codemode "), None).expect("parsed");
        assert_eq!(selection.add, vec!["codemode"]);

        assert!(ToolSelection::parse(Some("read,+grep"), None).is_err());
        assert!(
            ToolSelection::parse(Some("   "), None)
                .expect("blank")
                .is_default()
        );
    }

    #[test]
    fn an_entry_that_matches_nothing_is_reported() {
        let registry = registry();
        let selection = ToolSelection {
            only: vec!["read".to_string(), "teleport".to_string()],
            ..Default::default()
        };
        assert_eq!(registry.unmatched(&selection), vec!["teleport"]);
    }

    #[test]
    fn every_tool_describes_itself() {
        let registry = registry();
        for tool in registry.select(DEFAULTS, &ToolSelection::none()) {
            assert!(!tool.description().is_empty(), "{}", tool.name());
            assert_eq!(
                tool.parameters()["type"],
                "object",
                "{} takes an arguments object",
                tool.name()
            );
        }
        assert_eq!(
            registry.len(),
            BUILTIN_TOOLS.len() - 1,
            "eight tools are implemented, but only one shell can run on this platform"
        );
        assert!(!registry.is_empty());
    }
}
