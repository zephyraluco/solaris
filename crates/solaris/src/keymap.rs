//! Global keybindings and the footer hint text.

use solaris_tui::keys::Keybindings;

/// Hint shown in the footer when nothing else needs the space.
///
/// Kept short: the footer shares its row with the status segment, so a long
/// hint is the first thing to be truncated on an 80-column terminal.
pub const FOOTER_HINT: &str = "? help · ctrl+k commands · tab mode";

/// The default action → key bindings.
///
/// `ctrl+k` is reserved for the command palette, so the editor intentionally
/// does not bind kill-to-end-of-line.
pub fn default_bindings() -> Keybindings {
    let mut bindings = Keybindings::new();
    bindings.set("submit", "enter");
    bindings.set("newline", "alt+enter");
    bindings.set("quit", "ctrl+c");
    bindings.set("palette", "ctrl+k");
    bindings.set("help", "f1");
    bindings.set("toggle_mode", "tab");
    bindings.set("clear", "ctrl+l");
    bindings.set("theme", "ctrl+t");
    bindings.set("scroll_up", "pageup");
    bindings.set("scroll_down", "pagedown");
    bindings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn default_bindings_cover_the_documented_shortcuts() {
        let bindings = default_bindings();
        assert!(bindings.matches("quit", &key(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        assert!(bindings.matches("palette", &key(KeyCode::Char('k'), KeyModifiers::CONTROL)));
        assert!(bindings.matches("theme", &key(KeyCode::Char('t'), KeyModifiers::CONTROL)));
        assert!(bindings.matches("clear", &key(KeyCode::Char('l'), KeyModifiers::CONTROL)));
        assert!(bindings.matches("help", &key(KeyCode::F(1), KeyModifiers::empty())));
        assert!(bindings.matches("toggle_mode", &key(KeyCode::Tab, KeyModifiers::empty())));
    }

    #[test]
    fn palette_binding_does_not_collide_with_submit() {
        let bindings = default_bindings();
        let ctrl_k = key(KeyCode::Char('k'), KeyModifiers::CONTROL);
        assert!(!bindings.matches("submit", &ctrl_k));
        let enter = key(KeyCode::Enter, KeyModifiers::empty());
        assert!(!bindings.matches("palette", &enter));
    }
}
