//! A single-row link to a child agent's terminal, without disclosure state.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Clear, Widget};

use crate::component::{Component, KeyResult, MouseResult};
use crate::components::loader::SPINNER;
use crate::selection::SelectionHandle;
use crate::util::{display_width, rect_contains, truncate_to_width};

/// Summary activity is independent of the child's execution outcome.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SubagentSummaryStatus {
    Running,
    #[default]
    Complete,
}

pub const SUBAGENT_ICON: &str = "🤖";

/// A terminal link that emits an activation when clicked or confirmed.
///
/// Use [`Self::line`] in a transcript and [`Self::on_mouse`] with its visible
/// row rectangle. Retained layouts can use the [`Component`] implementation.
#[derive(Default)]
pub struct SubagentSummary {
    title: String,
    style: Style,
    status: SubagentSummaryStatus,
    spinner_frame: u16,
    area: Rect,
    pressed: Option<(u16, u16, Rect)>,
    activated: bool,
    selection: Option<SelectionHandle>,
}

impl SubagentSummary {
    pub fn new(title: impl Into<String>, style: Style) -> Self {
        Self {
            title: title.into(),
            style,
            ..Default::default()
        }
    }

    /// Refresh activity text without losing an in-progress pointer gesture.
    pub fn set_title(&mut self, title: impl Into<String>, style: Style) {
        self.title = title.into();
        self.style = style;
    }

    pub fn with_selection(mut self, selection: SelectionHandle) -> Self {
        self.selection = Some(selection);
        self
    }

    pub fn set_status(&mut self, status: SubagentSummaryStatus) {
        self.status = status;
    }

    pub fn set_spinner_frame(&mut self, frame: u16) {
        self.spinner_frame = frame;
    }

    fn marker(&self) -> &'static str {
        match self.status {
            SubagentSummaryStatus::Running => {
                SPINNER[usize::from(self.spinner_frame) % SPINNER.len()]
            }
            SubagentSummaryStatus::Complete => SUBAGENT_ICON,
        }
    }

    pub fn line(&self, width: u16) -> Line<'static> {
        let title: String = self
            .title
            .chars()
            .map(|ch| if ch.is_control() { ' ' } else { ch })
            .collect();
        Line::styled(
            truncate_to_width(
                &format!("  {} {title}", self.marker()),
                usize::from(width),
                "…",
            ),
            self.style,
        )
    }

    pub fn take_activated(&mut self) -> bool {
        std::mem::take(&mut self.activated)
    }

    pub fn cancel_interaction(&mut self) {
        self.pressed = None;
        self.activated = false;
    }

    /// Recognize a click without opening the terminal after a text-selection drag.
    pub fn on_mouse(
        &mut self,
        event: MouseEvent,
        area: Rect,
        selection_active: bool,
    ) -> MouseResult {
        let hit = rect_contains(area, event.column, event.row);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.pressed = hit.then_some((event.column, event.row, area));
                if hit {
                    MouseResult::Handled
                } else {
                    MouseResult::Ignored
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let Some((x, y, pressed_area)) = self.pressed.take() else {
                    return MouseResult::Ignored;
                };
                let marker_x = area.x.saturating_add(2);
                let marker = event.column >= marker_x
                    && event.column < marker_x.saturating_add(display_width(self.marker()) as u16);
                self.activated = hit
                    && area == pressed_area
                    && (x, y) == (event.column, event.row)
                    && (!selection_active || marker);
                MouseResult::Handled
            }
            MouseEventKind::Drag(MouseButton::Left)
            | MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => {
                self.cancel_interaction();
                MouseResult::Ignored
            }
            _ => MouseResult::Ignored,
        }
    }
}

impl Component for SubagentSummary {
    fn render(&mut self, buf: &mut Buffer, area: Rect) {
        let area = Rect {
            height: area.height.min(1),
            ..area
        };
        if self.area != area {
            self.cancel_interaction();
        }
        self.area = area;
        if area.is_empty() {
            return;
        }
        Clear.render(area, buf);
        buf.set_line(area.x, area.y, &self.line(area.width), area.width);
    }

    fn handle_key(&mut self, key: KeyEvent) -> KeyResult {
        if key.modifiers == KeyModifiers::empty()
            && matches!(key.code, KeyCode::Enter | KeyCode::Char(' '))
        {
            self.activated = true;
            KeyResult::Handled
        } else {
            KeyResult::Ignored
        }
    }

    fn handle_mouse(&mut self, event: MouseEvent) -> MouseResult {
        let selected = self
            .selection
            .as_ref()
            .is_some_and(|selection| selection.borrow().is_active());
        self.on_mouse(event, self.area, selected)
    }

    fn desired_height(&mut self, width: u16) -> Option<u16> {
        Some(u16::from(width > 0))
    }

    fn invalidate(&mut self) {
        self.cancel_interaction();
        self.area = Rect::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pointer(kind: MouseEventKind, column: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row: 4,
            modifiers: KeyModifiers::empty(),
        }
    }

    #[test]
    fn retained_and_transcript_renderers_match_and_fit_the_row() {
        let mut summary = SubagentSummary::new("Exploring: 中文 source\ncode", Style::default());
        for width in [1, 8, 24, 80] {
            let area = Rect::new(0, 0, width, 1);
            let mut actual = Buffer::empty(area);
            summary.render(&mut actual, area);
            let mut expected = Buffer::empty(area);
            expected.set_line(0, 0, &summary.line(width), width);
            assert_eq!(actual, expected);
            assert!(summary.line(width).width() <= usize::from(width));
        }
    }

    #[test]
    fn clicking_emits_one_activation_even_when_the_title_updates() {
        let mut summary = SubagentSummary::default();
        let area = Rect::new(0, 4, 80, 1);
        summary.on_mouse(
            pointer(MouseEventKind::Down(MouseButton::Left), 2),
            area,
            false,
        );
        summary.set_title("检查登录流程", Style::default());
        summary.on_mouse(
            pointer(MouseEventKind::Up(MouseButton::Left), 2),
            area,
            false,
        );
        assert!(summary.take_activated());
        assert!(!summary.take_activated());
    }

    #[test]
    fn dragging_selecting_scrolling_and_moving_rows_do_not_activate() {
        let area = Rect::new(0, 4, 80, 1);
        for (middle, release_area, selected) in [
            (Some(MouseEventKind::Drag(MouseButton::Left)), area, false),
            (Some(MouseEventKind::ScrollDown), area, false),
            (None, Rect::new(1, 4, 80, 1), false),
            (None, area, true),
        ] {
            let mut summary = SubagentSummary::default();
            summary.on_mouse(
                pointer(MouseEventKind::Down(MouseButton::Left), 8),
                area,
                false,
            );
            if let Some(kind) = middle {
                summary.on_mouse(pointer(kind, 8), area, false);
            }
            summary.on_mouse(
                pointer(MouseEventKind::Up(MouseButton::Left), 8),
                release_area,
                selected,
            );
            assert!(!summary.take_activated());
        }
    }

    #[test]
    fn running_summary_animates_and_completed_summary_uses_an_agent_icon() {
        let mut summary = SubagentSummary::new("检查登录流程", Style::default());
        summary.set_status(SubagentSummaryStatus::Running);
        summary.set_spinner_frame(0);
        assert!(summary.line(80).to_string().starts_with("  ⠋ "));
        summary.set_spinner_frame(1);
        assert!(summary.line(80).to_string().starts_with("  ⠙ "));
        summary.set_status(SubagentSummaryStatus::Complete);
        assert_eq!(summary.line(80).to_string(), "  🤖 检查登录流程");
    }

    #[test]
    fn enter_and_space_activate_but_arrows_do_not() {
        let mut summary = SubagentSummary::default();
        for code in [KeyCode::Enter, KeyCode::Char(' ')] {
            assert_eq!(
                summary.handle_key(KeyEvent::new(code, KeyModifiers::empty())),
                KeyResult::Handled
            );
            assert!(summary.take_activated());
        }
        assert_eq!(
            summary.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::empty())),
            KeyResult::Ignored
        );
    }
}
