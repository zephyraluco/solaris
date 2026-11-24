//! Default bottom composer container: multiline [`Editor`] + [`StatusBar`].

use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use solaris_tui::component::{Component, KeyResult, MouseResult};
use solaris_tui::components::editor::Editor;
use solaris_tui::layout::{self, Axis, Entry};
use solaris_tui::theme::Theme;

use super::status_bar::{StatusBar, StatusBarState};
use crate::tui::app::editor_styles;

/// Semantic action bubbled up from [`PromptContainer`] to the application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PromptAction {
    None,
    Submit(String),
    ToggleMode,
}

/// Retained container combining the prompt [`Editor`] and the 1-row [`StatusBar`].
pub(crate) struct PromptContainer {
    editor: Editor,
    status_bar: StatusBar,
    focused: bool,
    last_area: Rect,
}

impl PromptContainer {
    pub(crate) fn new(editor: Editor, theme: &Theme) -> Self {
        Self {
            editor,
            status_bar: StatusBar::new(theme),
            focused: true,
            last_area: Rect::default(),
        }
    }

    pub(crate) fn editor(&self) -> &Editor {
        &self.editor
    }

    pub(crate) fn editor_mut(&mut self) -> &mut Editor {
        &mut self.editor
    }

    pub(crate) fn sync_status(&mut self, state: StatusBarState) {
        self.status_bar.sync(state);
    }

    pub(crate) fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    pub(crate) fn set_theme(&mut self, theme: &Theme) {
        self.editor.set_styles(editor_styles(theme));
        self.status_bar.set_theme(theme);
    }

    pub(crate) fn desired_height(&mut self, width: u16) -> u16 {
        let editor_height = self.editor.desired_height(width).unwrap_or(3).max(3);
        let status_height = self.status_bar.desired_height(width).unwrap_or(1);
        editor_height.saturating_add(status_height)
    }

    pub(crate) fn render(&mut self, buf: &mut Buffer, area: Rect) {
        self.last_area = area;
        if area.width == 0 || area.height == 0 {
            return;
        }
        let rows = layout::split(
            area,
            Axis::Vertical,
            &[Entry::auto().grow(1).min(1), Entry::px(1)],
        );
        self.editor.set_focused(self.focused);
        self.editor.render(buf, rows[0]);
        self.status_bar.render(buf, rows[1]);
    }

    pub(crate) fn cursor_position(&self) -> Option<(u16, u16)> {
        self.editor.cursor_position()
    }

    pub(crate) fn handle_key(
        &mut self,
        key: KeyEvent,
        is_toggle_mode: bool,
    ) -> (KeyResult, PromptAction) {
        let result = self.editor.handle_key(key);
        if let Some(text) = self.editor.take_submitted() {
            return (KeyResult::Handled, PromptAction::Submit(text));
        }
        if result.is_handled() {
            return (result, PromptAction::None);
        }
        // The editor ignores Tab when its completion popup is closed.
        if is_toggle_mode {
            return (KeyResult::Handled, PromptAction::ToggleMode);
        }
        (result, PromptAction::None)
    }

    pub(crate) fn handle_mouse(&mut self, event: MouseEvent) -> MouseResult {
        self.editor.handle_mouse(event)
    }

    pub(crate) fn handle_paste(&mut self, text: &str) {
        self.editor.handle_paste(text);
    }
}
