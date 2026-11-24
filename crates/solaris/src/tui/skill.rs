//! The `/skills` inline picker: choose a skill and insert `@<skill> ` into the prompt.
//!
//! Styled identically to `/connect` and `/model` via [`InlineSelect`] and hosted
//! as a peer inside [`BottomDock`](crate::tui::containers::bottom_dock::BottomDock).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use solaris_agent::Skills;
use solaris_tui::components::inline_select::{InlineSelect, InlineSelectOutcome, InlineStyles};
use solaris_tui::components::select_list::SelectItem;
use solaris_tui::theme::Theme;

/// What a key or mouse event did to the skill picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillOutcome {
    /// Consumed; the picker stays up.
    Handled,
    /// The picker was dismissed without choosing a skill.
    Closed,
    /// A skill was chosen and should be inserted as `@<skill_name> `.
    Picked { skill_name: String },
}

/// Inline skill selector rendered in `BottomDock`.
pub struct SkillPicker {
    picker: InlineSelect,
}

impl SkillPicker {
    pub fn new(theme: &Theme, skills: &Skills) -> Self {
        let items: Vec<SelectItem> = skills
            .iter()
            .map(|skill| {
                let description = format!(
                    "[{}] {}{}",
                    skill.source().badge(),
                    skill.description_inline(),
                    if skill.disable_model_invocation() {
                        " (explicit only)"
                    } else {
                        ""
                    }
                );
                SelectItem::new(skill.name(), skill.name()).description(description)
            })
            .collect();
        let picker = InlineSelect::new(
            InlineStyles::from_theme(theme),
            "Skills",
            "Select a skill:",
            items,
        );
        Self { picker }
    }

    pub fn set_theme(&mut self, theme: &Theme) {
        self.picker.set_styles(InlineStyles::from_theme(theme));
    }

    pub fn area(&self) -> Rect {
        self.picker.area()
    }

    pub fn desired_height(&self, width: u16) -> u16 {
        self.picker.desired_height(width)
    }

    pub fn insert_paste(&mut self, _data: &str) -> bool {
        false
    }

    pub fn on_key(&mut self, key: KeyEvent) -> SkillOutcome {
        if key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            return SkillOutcome::Closed;
        }
        match self.picker.on_key(key) {
            InlineSelectOutcome::Picked(_) => self.picked(),
            InlineSelectOutcome::Handled => SkillOutcome::Handled,
        }
    }

    pub fn on_mouse(&mut self, mouse: MouseEvent) -> SkillOutcome {
        match self.picker.on_mouse(mouse) {
            InlineSelectOutcome::Picked(_) => self.picked(),
            InlineSelectOutcome::Handled => SkillOutcome::Handled,
        }
    }

    pub fn render(&mut self, buf: &mut Buffer, area: Rect) {
        self.picker.render(buf, area);
    }

    fn picked(&self) -> SkillOutcome {
        match self.picker.selected_value() {
            Some(name) => SkillOutcome::Picked {
                skill_name: name.to_string(),
            },
            None => SkillOutcome::Handled,
        }
    }
}
