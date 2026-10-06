//! Key matching helpers and a small action/keybinding table.
//!
//! Follows pi-tui's approach: components compare decoded key events against
//! named key specs instead of matching raw escape bytes.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Exact match on code and modifiers.
pub fn matches(key: &KeyEvent, code: KeyCode, modifiers: KeyModifiers) -> bool {
    key.code == code && key.modifiers == modifiers
}

/// `Ctrl+<c>` (case-insensitive; crossterm reports control chars as uppercase).
pub fn ctrl(key: &KeyEvent, c: char) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char(ch) if ch.eq_ignore_ascii_case(&c))
}

/// `Alt+<c>`.
pub fn alt(key: &KeyEvent, c: char) -> bool {
    key.modifiers.contains(KeyModifiers::ALT)
        && matches!(key.code, KeyCode::Char(ch) if ch.eq_ignore_ascii_case(&c))
}

/// A plain character with no modifier (Shift allowed), returning it.
pub fn plain_char(key: &KeyEvent) -> Option<char> {
    let blocked = KeyModifiers::CONTROL | KeyModifiers::ALT;
    if key.modifiers.intersects(blocked) {
        return None;
    }
    match key.code {
        KeyCode::Char(c) => Some(c),
        _ => None,
    }
}

/// Enter without Alt/Ctrl submits the prompt.
pub fn is_submit(key: &KeyEvent) -> bool {
    key.code == KeyCode::Enter
        && !key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL)
}

/// Alt+Enter / Ctrl+Enter insert a newline instead of submitting.
pub fn is_newline(key: &KeyEvent) -> bool {
    key.code == KeyCode::Enter
        && key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL)
}

/// Parse a key spec such as `ctrl+k`, `shift+tab`, `alt+enter` or `f1`.
pub fn parse_key_spec(spec: &str) -> Option<(KeyCode, KeyModifiers)> {
    let mut modifiers = KeyModifiers::empty();
    let mut code: Option<KeyCode> = None;

    for part in spec.split('+') {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => modifiers |= KeyModifiers::CONTROL,
            "alt" | "option" => modifiers |= KeyModifiers::ALT,
            "shift" => modifiers |= KeyModifiers::SHIFT,
            "enter" | "return" => code = Some(KeyCode::Enter),
            "esc" | "escape" => code = Some(KeyCode::Esc),
            "tab" => code = Some(KeyCode::Tab),
            "space" => code = Some(KeyCode::Char(' ')),
            "backspace" => code = Some(KeyCode::Backspace),
            "delete" | "del" => code = Some(KeyCode::Delete),
            "insert" | "ins" => code = Some(KeyCode::Insert),
            "home" => code = Some(KeyCode::Home),
            "end" => code = Some(KeyCode::End),
            "up" => code = Some(KeyCode::Up),
            "down" => code = Some(KeyCode::Down),
            "left" => code = Some(KeyCode::Left),
            "right" => code = Some(KeyCode::Right),
            "pageup" | "pgup" => code = Some(KeyCode::PageUp),
            "pagedown" | "pgdn" => code = Some(KeyCode::PageDown),
            other => {
                let mut chars = other.chars();
                let first = chars.next()?;
                if chars.next().is_some() {
                    // Try function keys like f1..f12.
                    if let Some(num) = other.strip_prefix('f').and_then(|n| n.parse::<u8>().ok())
                        && (1..=12).contains(&num)
                    {
                        code = Some(KeyCode::F(num));
                        continue;
                    }
                    return None;
                }
                code = Some(KeyCode::Char(first));
            }
        }
    }

    code.map(|code| (code, modifiers))
}

/// Action → key-spec bindings with sensible defaults.
#[derive(Debug, Clone)]
pub struct Keybindings {
    entries: Vec<(String, String)>,
}

impl Default for Keybindings {
    fn default() -> Self {
        Self::new()
    }
}

impl Keybindings {
    /// Default bindings.
    pub fn new() -> Self {
        Self {
            entries: vec![
                ("submit".into(), "enter".into()),
                ("newline".into(), "alt+enter".into()),
                ("quit".into(), "ctrl+c".into()),
                ("palette".into(), "ctrl+k".into()),
                ("help".into(), "f1".into()),
                ("toggle_mode".into(), "tab".into()),
                ("clear".into(), "ctrl+l".into()),
                ("theme".into(), "ctrl+t".into()),
                ("scroll_up".into(), "pageup".into()),
                ("scroll_down".into(), "pagedown".into()),
            ],
        }
    }

    /// Override (or add) a binding.
    pub fn set(&mut self, action: impl Into<String>, spec: impl Into<String>) {
        let action = action.into();
        let spec = spec.into();
        if let Some(entry) = self.entries.iter_mut().find(|(a, _)| *a == action) {
            entry.1 = spec;
        } else {
            self.entries.push((action, spec));
        }
    }

    /// The key spec bound to `action`.
    pub fn spec(&self, action: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(a, _)| a == action)
            .map(|(_, spec)| spec.as_str())
    }

    /// Whether `key` triggers `action`.
    pub fn matches(&self, action: &str, key: &KeyEvent) -> bool {
        let Some(spec) = self.spec(action) else {
            return false;
        };
        let Some((code, modifiers)) = parse_key_spec(spec) else {
            return false;
        };
        let relevant = KeyModifiers::CONTROL | KeyModifiers::ALT;
        if modifiers.is_empty() {
            // Bare keys must not carry Ctrl/Alt, but Shift is tolerated because
            // many terminals report it for shifted letters.
            !key.modifiers.intersects(relevant) && key.code == code
        } else {
            key.code == code && key.modifiers.contains(modifiers)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn parses_simple_and_modified_specs() {
        assert_eq!(
            parse_key_spec("enter"),
            Some((KeyCode::Enter, KeyModifiers::empty()))
        );
        assert_eq!(
            parse_key_spec("ctrl+k"),
            Some((KeyCode::Char('k'), KeyModifiers::CONTROL))
        );
        assert_eq!(
            parse_key_spec("shift+tab"),
            Some((KeyCode::Tab, KeyModifiers::SHIFT))
        );
        assert_eq!(
            parse_key_spec("f1"),
            Some((KeyCode::F(1), KeyModifiers::empty()))
        );
        assert_eq!(parse_key_spec("nope+nope"), None);
    }

    #[test]
    fn distinguishes_submit_from_newline() {
        assert!(is_submit(&key(KeyCode::Enter, KeyModifiers::empty())));
        assert!(!is_submit(&key(KeyCode::Enter, KeyModifiers::ALT)));
        assert!(is_newline(&key(KeyCode::Enter, KeyModifiers::ALT)));
        assert!(!is_newline(&key(KeyCode::Enter, KeyModifiers::empty())));
    }

    #[test]
    fn plain_char_rejects_modifiers() {
        assert_eq!(
            plain_char(&key(KeyCode::Char('a'), KeyModifiers::empty())),
            Some('a')
        );
        assert_eq!(
            plain_char(&key(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(
            plain_char(&key(KeyCode::Enter, KeyModifiers::empty())),
            None
        );
    }

    #[test]
    fn keybindings_match_defaults_and_overrides() {
        let mut bindings = Keybindings::new();
        assert!(bindings.matches("quit", &key(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        assert!(!bindings.matches("quit", &key(KeyCode::Char('q'), KeyModifiers::CONTROL)));
        assert!(bindings.matches("submit", &key(KeyCode::Enter, KeyModifiers::empty())));

        bindings.set("quit", "ctrl+q");
        assert!(bindings.matches("quit", &key(KeyCode::Char('q'), KeyModifiers::CONTROL)));
        assert!(!bindings.matches("quit", &key(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        assert!(!bindings.matches("missing", &key(KeyCode::Char('c'), KeyModifiers::CONTROL)));
    }
}
