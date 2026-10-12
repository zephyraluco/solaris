//! The slash commands the prompt accepts, and the views built over them.
//!
//! [`PROMPT_SLASH_COMMANDS`] is the single source of truth: parsing, the
//! editor's completion popup, the Ctrl+K palette and the help dialog all read
//! this one table rather than each keeping a list of their own. Running a
//! command is deliberately not here — the table is data, and
//! `App::execute_command` is what decides what each name does.

use solaris_tui::components::editor::CommandHint;
use solaris_tui::components::select_list::SelectItem;

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
        description: "Toggle build / plan mode",
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
pub fn hints() -> Vec<CommandHint> {
    PROMPT_SLASH_COMMANDS
        .iter()
        .map(|spec| CommandHint::new(spec.name, spec.description, spec.args))
        .collect()
}

/// Items for the Ctrl+K command palette.
pub fn palette_items() -> Vec<SelectItem> {
    PROMPT_SLASH_COMMANDS
        .iter()
        .map(|spec| {
            let item = SelectItem::new(spec.name, format!("/{}", spec.name));
            if spec.args.is_empty() {
                item.description(spec.description)
            } else {
                item.description(format!("{} — {}", spec.description, spec.args))
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn hints_cover_every_command() {
        let hints = hints();
        assert_eq!(hints.len(), PROMPT_SLASH_COMMANDS.len());
        for (hint, spec) in hints.iter().zip(PROMPT_SLASH_COMMANDS) {
            assert_eq!(hint.name, spec.name);
            assert_eq!(hint.description, spec.description);
            assert_eq!(hint.args, spec.args);
        }
    }

    #[test]
    fn palette_items_are_every_command_named_with_a_slash() {
        let items = palette_items();
        assert_eq!(items.len(), PROMPT_SLASH_COMMANDS.len());
        for (item, spec) in items.iter().zip(PROMPT_SLASH_COMMANDS) {
            // The value is fed back through `App::submit`, which re-adds the
            // slash; the label is what the palette shows.
            assert_eq!(item.value, spec.name);
            assert_eq!(item.label, format!("/{}", spec.name));
        }
    }
}
