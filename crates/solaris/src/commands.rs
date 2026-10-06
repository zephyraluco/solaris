//! Slash-command registry: editor hints and the command palette.
//!
//! The command table itself lives in `solaris-core` so parsing, completion, help and
//! palette all read from one source of truth.

use solaris_core::{PROMPT_SLASH_COMMANDS, SlashCommandSpec};
use solaris_tui::components::editor::CommandHint;
use solaris_tui::components::select_list::SelectItem;

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

/// Look up a command by name.
pub fn find(name: &str) -> Option<&'static SlashCommandSpec> {
    PROMPT_SLASH_COMMANDS.iter().find(|spec| spec.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn palette_values_are_bare_command_names() {
        for item in palette_items() {
            assert!(
                !item.value.starts_with('/'),
                "{} leaked a slash",
                item.value
            );
            assert!(find(&item.value).is_some());
        }
    }

    #[test]
    fn find_is_exact() {
        assert!(find("help").is_some());
        assert!(find("hel").is_none());
        assert!(find("/help").is_none());
    }
}
