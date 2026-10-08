//! The `/model` command: pick the active model from a list.
//!
//! Like `/connect` it takes over the prompt region instead of opening an
//! overlay, and it draws through the framework's [`InlineSelect`] — the same
//! code the wizard's model step uses, so the two cannot drift apart.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use solaris_tui::components::inline_select::{InlineSelect, InlineSelectOutcome, InlineStyles};
use solaris_tui::components::select_list::SelectItem;
use solaris_tui::theme::Theme;

/// What a key or mouse event did to the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelOutcome {
    /// Consumed; the picker stays up.
    Handled,
    /// The picker is finished — the application must drop it.
    Closed,
    /// A model was confirmed as the new active one.
    Picked { model_id: String },
}

/// The `/model` picker.
pub struct ModelPicker {
    picker: InlineSelect,
}

impl ModelPicker {
    /// A picker over `items`, highlighting `current` when it is on the list.
    pub fn new(theme: &Theme, items: Vec<SelectItem>, current: Option<&str>) -> Self {
        let mut picker = InlineSelect::new(
            InlineStyles::from_theme(theme),
            "Model",
            "Select a model:",
            items,
        );
        if let Some(current) = current {
            picker.select_value(current);
        }
        Self { picker }
    }

    /// Repaint with another theme's palette.
    pub fn set_theme(&mut self, theme: &Theme) {
        self.picker.set_styles(InlineStyles::from_theme(theme));
    }

    /// The area the picker last drew into.
    pub fn area(&self) -> Rect {
        self.picker.area()
    }

    /// Rows the picker wants at `width`.
    pub fn desired_height(&self, width: u16) -> u16 {
        self.picker.desired_height(width)
    }

    /// Smallest height the picker can still be used in.
    pub fn min_height(&self) -> u16 {
        4
    }

    /// The highlighted model.
    pub fn selected_model(&self) -> Option<&str> {
        self.picker.selected_value()
    }

    /// Drop a paste: the picker has no text field to take it.
    pub fn insert_paste(&mut self, _data: &str) -> bool {
        false
    }

    /// Route a key.
    pub fn on_key(&mut self, key: KeyEvent) -> ModelOutcome {
        // Esc — and Ctrl+C, the instinctive "abort this prompt" — closes it.
        if key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            return ModelOutcome::Closed;
        }

        match self.picker.on_key(key) {
            InlineSelectOutcome::Picked(_) => self.picked(),
            InlineSelectOutcome::Handled => ModelOutcome::Handled,
        }
    }

    /// Route a mouse event.
    pub fn on_mouse(&mut self, mouse: MouseEvent) -> ModelOutcome {
        match self.picker.on_mouse(mouse) {
            InlineSelectOutcome::Picked(_) => self.picked(),
            InlineSelectOutcome::Handled => ModelOutcome::Handled,
        }
    }

    /// Draw the picker into `area`.
    pub fn render(&mut self, buf: &mut Buffer, area: Rect) {
        self.picker.render(buf, area);
    }

    /// The row the highlight sits on, as an outcome.
    fn picked(&self) -> ModelOutcome {
        match self.picker.selected_value() {
            Some(model_id) => ModelOutcome::Picked {
                model_id: model_id.to_string(),
            },
            None => ModelOutcome::Handled,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items() -> Vec<SelectItem> {
        vec![
            SelectItem::new("model-a", "model-a").description("the default"),
            SelectItem::new("model-b", "model-b").description("the quick one"),
        ]
    }

    fn picker(current: Option<&str>) -> ModelPicker {
        ModelPicker::new(&Theme::dark(), items(), current)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    #[test]
    fn the_active_model_starts_highlighted() {
        assert_eq!(picker(Some("model-b")).selected_model(), Some("model-b"));
    }

    #[test]
    fn an_unknown_active_model_falls_back_to_the_first_row() {
        assert_eq!(picker(Some("gone")).selected_model(), Some("model-a"));
    }

    #[test]
    fn enter_takes_the_highlighted_model() {
        let mut picker = picker(Some("model-b"));

        assert_eq!(
            picker.on_key(key(KeyCode::Enter)),
            ModelOutcome::Picked {
                model_id: "model-b".into()
            }
        );
    }

    #[test]
    fn arrows_move_and_digits_jump() {
        let mut picker = picker(Some("model-a"));
        assert_eq!(picker.on_key(key(KeyCode::Down)), ModelOutcome::Handled);
        assert_eq!(picker.selected_model(), Some("model-b"));

        assert_eq!(
            picker.on_key(key(KeyCode::Char('1'))),
            ModelOutcome::Picked {
                model_id: "model-a".into()
            }
        );
    }

    #[test]
    fn esc_and_ctrl_c_close_it() {
        let mut picker = picker(None);

        assert_eq!(picker.on_key(key(KeyCode::Esc)), ModelOutcome::Closed);
        assert_eq!(
            picker.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            ModelOutcome::Closed
        );
    }

    #[test]
    fn the_picker_never_takes_a_paste() {
        assert!(!picker(None).insert_paste("text"));
    }
}
