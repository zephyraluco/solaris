//! The slash commands the prompt accepts, and the views built over them.
//!
//! [`PROMPT_SLASH_COMMANDS`] is the single source of truth: parsing, the
//! editor's completion popup, and the help dialog all read
//! this one table rather than each keeping a list of their own. Running a
//! command is deliberately not here — the table is data, and
//! `App::execute_command` is what decides what each name does.
//!
//! Skills are the one thing that does not fit a fixed table: a skill is not one
//! command but one per skill, so the table carries the `skill:` prefix alone and
//! [`hints`] expands it into a row per skill for the editor.

use solaris_agent::Skills;
use solaris_tui::components::editor::CommandHint;

/// What an invocation is written as: `/skill:<name>`.
///
/// The colon rather than a space, because the name is the part being completed —
/// it lets the editor offer every skill once `/skill` has been typed, the same
/// way [pi](https://github.com/earendil-works/pi) spells it.
pub const SKILL_PREFIX: &str = "skill:";

// --------------------------------------------------------------- the table

/// A parsed slash command: `/name args…`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCommand {
    pub name: String,
    pub args: String,
}

/// Static description of a prompt slash command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlashCommandSpec {
    pub name: &'static str,
    pub description: &'static str,
    /// Argument hint, empty when the command takes none.
    pub args: &'static str,
}

/// Every slash command the prompt accepts.
pub const PROMPT_SLASH_COMMANDS: &[SlashCommandSpec] = &[
    SlashCommandSpec {
        name: "help",
        description: "Show keyboard shortcuts and commands",
        args: "",
    },
    SlashCommandSpec {
        name: "connect",
        description: "Connect a model provider",
        args: "",
    },
    SlashCommandSpec {
        name: "theme",
        description: "Switch the colour theme",
        args: "[dark|light]",
    },
    SlashCommandSpec {
        name: "buddy",
        description: "Show your companion",
        args: "[name <name>]",
    },
    SlashCommandSpec {
        name: "model",
        description: "Switch the active model",
        args: "[name]",
    },
    SlashCommandSpec {
        name: "mode",
        description: "Switch mode (default / plan / auto / ask)",
        args: "[default|plan|auto|ask]",
    },
    SlashCommandSpec {
        name: "compact",
        description: "Summarize the conversation to free up context",
        args: "",
    },
    SlashCommandSpec {
        name: "queue",
        description: "Run a prompt after the turn in flight",
        args: "<prompt>",
    },
    SlashCommandSpec {
        name: "reload",
        description: "Reload skills and project instructions",
        args: "",
    },
    SlashCommandSpec {
        name: "skills",
        description: "Select a skill",
        args: "",
    },
    SlashCommandSpec {
        name: "clear",
        description: "Clear the transcript",
        args: "",
    },
    SlashCommandSpec {
        name: "stats",
        description: "Show token and cost statistics",
        args: "",
    },
    SlashCommandSpec {
        name: "quit",
        description: "Exit solaris",
        args: "",
    },
    SlashCommandSpec {
        name: "subagents",
        description: "List active and queued child task IDs",
        args: "",
    },
    SlashCommandSpec {
        name: "stop",
        description: "Cancel one child without stopping its parent",
        args: "<task-id>",
    },
    SlashCommandSpec {
        name: "sessions",
        description: "Choose a saved session",
        args: "",
    },
    SlashCommandSpec {
        name: "resume",
        description: "Resume a saved session",
        args: "[session-id]",
    },
    SlashCommandSpec {
        name: "fork",
        description: "Fork the current saved conversation",
        args: "",
    },
    SlashCommandSpec {
        name: "tree",
        description: "Show session and child relationships",
        args: "",
    },
];

// ------------------------------------------------------------ the parsing

/// Parse `input` as a slash command.
///
/// Returns `None` unless the first non-whitespace character is `/` and a
/// non-empty name follows.
pub fn parse_slash_command(input: &str) -> Option<SlashCommand> {
    let trimmed = input.trim_start();
    let body = trimmed.strip_prefix('/')?;
    if body.is_empty() {
        return None;
    }

    let mut parts = body.splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or_default();
    if name.is_empty() {
        return None;
    }

    Some(SlashCommand {
        name: name.to_string(),
        args: parts.next().unwrap_or_default().trim().to_string(),
    })
}

// --------------------------------------------------------------- the views

/// Completion hints for the editor's slash-command popup.
///
/// Only top-level slash commands (including `/skills`) are shown here; choosing
/// `/skills` opens the inline skill picker to insert `@<skill> `.
pub fn hints(_skills: &Skills) -> Vec<CommandHint> {
    PROMPT_SLASH_COMMANDS
        .iter()
        .map(|spec| CommandHint::new(spec.name, spec.description, spec.args))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use solaris_agent::{SkillRoot, SkillSource};

    /// A skills set holding one skill called `name`, found in a directory of its
    /// own — the loader is the only way to build one from outside its crate.
    fn skills_holding(name: &str, description: &str) -> Skills {
        let dir = std::env::temp_dir().join(format!("solaris-commands-{name}"));
        let path = dir.join(name).join("SKILL.md");
        std::fs::create_dir_all(path.parent().expect("a skill directory")).expect("skills dir");
        std::fs::write(&path, format!("---\ndescription: {description}\n---\n")).expect("write");
        Skills::load(&[SkillRoot::new(&dir, SkillSource::Project)], &[])
    }

    #[test]
    fn parses_name_and_args() {
        let cmd = parse_slash_command("/theme light").expect("parsed");
        assert_eq!(cmd.name, "theme");
        assert_eq!(cmd.args, "light");
    }

    #[test]
    fn parses_command_without_args() {
        let cmd = parse_slash_command("/help").expect("parsed");
        assert_eq!(cmd.name, "help");
        assert_eq!(cmd.args, "");
    }

    #[test]
    fn tolerates_leading_whitespace() {
        let cmd = parse_slash_command("   /model claude-opus-4-1  ").expect("parsed");
        assert_eq!(cmd.name, "model");
        assert_eq!(cmd.args, "claude-opus-4-1");
    }

    #[test]
    fn rejects_non_commands() {
        assert!(parse_slash_command("hello").is_none());
        assert!(parse_slash_command("/").is_none());
        assert!(parse_slash_command("   ").is_none());
    }

    #[test]
    fn command_table_is_well_formed() {
        for spec in PROMPT_SLASH_COMMANDS {
            assert!(!spec.name.is_empty());
            assert!(!spec.description.is_empty());
            assert!(!spec.name.starts_with('/'));
        }
    }

    #[test]
    fn hints_only_include_top_level_slash_commands() {
        let skills = skills_holding("pdf-tools", "Extract text from PDFs.");
        let hints = hints(&skills);

        assert_eq!(hints.len(), PROMPT_SLASH_COMMANDS.len());
        for (hint, spec) in hints.iter().zip(PROMPT_SLASH_COMMANDS) {
            assert_eq!(hint.name, spec.name);
            assert_eq!(hint.description, spec.description);
            assert_eq!(hint.args, spec.args);
        }
        assert!(
            hints
                .iter()
                .all(|hint| !hint.name.starts_with(SKILL_PREFIX)),
            "individual skills must not be expanded into /skill:<name> hints: {:?}",
            hints.iter().map(|hint| &hint.name).collect::<Vec<_>>()
        );
        assert!(hints.iter().any(|hint| hint.name == "skills"));
    }
}
