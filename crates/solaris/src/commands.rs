//! The slash commands the prompt accepts, and the views built over them.
//!
//! [`PROMPT_SLASH_COMMANDS`] is the single source of truth: parsing, the
//! editor's completion popup, the Ctrl+K palette and the help dialog all read
//! this one table rather than each keeping a list of their own. Running a
//! command is deliberately not here — the table is data, and
//! `App::execute_command` is what decides what each name does.
//!
//! Skills are the one thing that does not fit a fixed table: a skill is not one
//! command but one per skill, so the table carries the `skill:` prefix alone and
//! [`hints`] expands it into a row per skill for the editor.

use solaris_agent::{Skill, Skills};
use solaris_tui::components::editor::CommandHint;
use solaris_tui::components::select_list::SelectItem;

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
        description: "Toggle build / plan mode",
        args: "",
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
        name: "skills",
        description: "List the skills available",
        args: "",
    },
    SlashCommandSpec {
        name: SKILL_PREFIX,
        description: "Load a skill's instructions",
        args: "<name> [request]",
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
///
/// The `skill:` entry is left out here even though `/help` and the palette carry
/// it: a placeholder for a name the editor can list itself is worse than the
/// names. Typing `/skill` offers every skill by name, with what it is for and
/// where it came from, and picking one completes the whole invocation.
pub fn hints(skills: &Skills) -> Vec<CommandHint> {
    let mut hints: Vec<CommandHint> = PROMPT_SLASH_COMMANDS
        .iter()
        .filter(|spec| spec.name != SKILL_PREFIX)
        .map(|spec| CommandHint::new(spec.name, spec.description, spec.args))
        .collect();
    hints.extend(skills.visible().map(skill_hint));
    hints
}

/// One skill as a completion row.
///
/// The badge comes first so the descriptions line up behind it, and the
/// invocation carries an `args`, which is what makes the editor leave a space
/// after it for the request.
fn skill_hint(skill: &Skill) -> CommandHint {
    CommandHint::new(
        format!("{SKILL_PREFIX}{}", skill.name()),
        format!(
            "[{}] {}",
            skill.source().badge(),
            skill.description_inline()
        ),
        "[request]",
    )
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
    fn hints_cover_every_command_that_is_not_a_skill() {
        let hints = hints(&Skills::empty());
        let expected: Vec<&SlashCommandSpec> = PROMPT_SLASH_COMMANDS
            .iter()
            .filter(|spec| spec.name != SKILL_PREFIX)
            .collect();

        assert_eq!(hints.len(), expected.len());
        for (hint, spec) in hints.iter().zip(expected) {
            assert_eq!(hint.name, spec.name);
            assert_eq!(hint.description, spec.description);
            assert_eq!(hint.args, spec.args);
        }
    }

    #[test]
    fn hints_offer_one_badged_row_per_skill_and_no_placeholder() {
        let skills = skills_holding("pdf-tools", "Extract text from PDFs.");
        let hints = hints(&skills);

        assert!(
            hints.iter().all(|hint| hint.name != SKILL_PREFIX),
            "`skill:` is a prefix, not a row: {:?}",
            hints.iter().map(|hint| &hint.name).collect::<Vec<_>>()
        );

        let hint = hints.last().expect("a row for the skill");
        assert_eq!(hint.name, "skill:pdf-tools");
        assert_eq!(hint.description, "[p] Extract text from PDFs.");
        assert_eq!(
            hint.args, "[request]",
            "an args is what makes the editor leave room for the request"
        );
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
