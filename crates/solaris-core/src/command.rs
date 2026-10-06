//! Slash-command parsing and the prompt command table.
//!
//! Parsing is a pure function so it can live in the bottom crate and be reused
//! by the editor's autocomplete, the command palette, and the app executor.

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

/// Every slash command the prompt accepts. Single source of truth for the
/// command palette, the help dialog, and the editor autocomplete.
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

/// Commands whose name starts with `prefix` (case-insensitive).
///
/// An empty prefix matches every command.
pub fn matching_slash_commands(prefix: &str) -> Vec<&'static SlashCommandSpec> {
    let prefix = prefix.to_ascii_lowercase();
    PROMPT_SLASH_COMMANDS
        .iter()
        .filter(|spec| spec.name.starts_with(&prefix))
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
        let cmd = parse_slash_command("   /model solaris-mock-1  ").expect("parsed");
        assert_eq!(cmd.name, "model");
        assert_eq!(cmd.args, "solaris-mock-1");
    }

    #[test]
    fn rejects_non_commands() {
        assert!(parse_slash_command("hello").is_none());
        assert!(parse_slash_command("/").is_none());
        assert!(parse_slash_command("   ").is_none());
    }

    #[test]
    fn filters_by_prefix() {
        let names: Vec<_> = matching_slash_commands("m")
            .iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, vec!["model", "mode"]);

        assert_eq!(
            matching_slash_commands("").len(),
            PROMPT_SLASH_COMMANDS.len()
        );
        assert!(matching_slash_commands("zzz").is_empty());
    }

    #[test]
    fn command_table_is_well_formed() {
        for spec in PROMPT_SLASH_COMMANDS {
            assert!(!spec.name.is_empty());
            assert!(!spec.description.is_empty());
            assert!(!spec.name.starts_with('/'));
        }
    }
}
