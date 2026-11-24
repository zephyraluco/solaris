//! Bottom dock container hosting peer interactive surfaces below the transcript.
//!
//! [`PromptContainer`], [`QuestionPanel`], and [`Inline`] (`/connect`, `/model`)
//! are peers at the same hierarchy level. Activating a question or inline flow
//! replaces the entire [`PromptContainer`] (both editor and status bar) while
//! keeping the prompt's draft, history, and cursor intact for when it returns.

use std::collections::VecDeque;

use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use solaris_tui::component::{Component, KeyResult, MouseResult};
use solaris_tui::components::editor::Editor;
use solaris_tui::theme::Theme;
use solaris_tui::util::rect_contains;

use super::approval::{ApprovalOutcome, ApprovalPanel};
use super::prompt::{PromptAction, PromptContainer};
use super::status_bar::StatusBarState;
use crate::tui::inline::{Inline, InlineOutcome};
use crate::tui::view::question::QuestionPanel;

/// Which peer surface currently owns [`BottomDock`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BottomSurface {
    Prompt,
    Question,
    Approval,
    Inline,
}

/// Semantic outcome bubbled up from [`BottomDock`] to `App`.
pub(crate) enum BottomAction {
    None,
    Prompt(PromptAction),
    Inline(InlineOutcome),
    Approval { id: u64, accepted: bool },
    QuestionClosed,
}

const TOP_DIVIDER_ROWS: u16 = 1;

/// Peer-switching container below the transcript pane.
pub(crate) struct BottomDock {
    prompt: PromptContainer,
    approvals: VecDeque<ApprovalPanel>,
    question: Option<QuestionPanel>,
    inline: Option<Inline>,
    divider_style: Style,
    last_area: Rect,
}

impl BottomDock {
    pub(crate) fn new(editor: Editor, theme: &Theme) -> Self {
        Self {
            prompt: PromptContainer::new(editor, theme),
            approvals: VecDeque::new(),
            question: None,
            inline: None,
            divider_style: Style::default().fg(theme.accent),
            last_area: Rect::default(),
        }
    }

    pub(crate) fn active_surface(&self) -> BottomSurface {
        if self.inline.is_some() {
            BottomSurface::Inline
        } else if !self.approvals.is_empty() {
            BottomSurface::Approval
        } else if self.question.is_some() {
            BottomSurface::Question
        } else {
            BottomSurface::Prompt
        }
    }

    pub(crate) fn is_prompt_active(&self) -> bool {
        self.active_surface() == BottomSurface::Prompt
    }

    pub(crate) fn editor(&self) -> &Editor {
        self.prompt.editor()
    }

    pub(crate) fn editor_mut(&mut self) -> &mut Editor {
        self.prompt.editor_mut()
    }

    pub(crate) fn push_approval(&mut self, panel: ApprovalPanel) {
        self.approvals.push_back(panel);
    }

    pub(crate) fn remove_approval(&mut self, id: u64) {
        self.approvals.retain(|panel| panel.id() != id);
    }

    pub(crate) fn question(&self) -> Option<&QuestionPanel> {
        self.question.as_ref()
    }

    pub(crate) fn set_question(&mut self, panel: Option<QuestionPanel>) {
        self.question = panel;
    }

    pub(crate) fn clear_question(&mut self) {
        self.question = None;
    }

    pub(crate) fn inline(&self) -> Option<&Inline> {
        self.inline.as_ref()
    }

    pub(crate) fn inline_mut(&mut self) -> Option<&mut Inline> {
        self.inline.as_mut()
    }

    pub(crate) fn set_inline(&mut self, flow: Option<Inline>) {
        self.inline = flow;
    }

    pub(crate) fn clear_inline(&mut self) {
        self.inline = None;
    }

    pub(crate) fn sync_prompt(&mut self, state: StatusBarState, focused: bool) {
        self.prompt.sync_status(state);
        self.prompt.set_focused(focused);
    }

    pub(crate) fn set_theme(&mut self, theme: &Theme) {
        self.divider_style = Style::default().fg(theme.accent);
        self.prompt.set_theme(theme);
        for approval in &mut self.approvals {
            approval.set_theme(theme);
        }
        if let Some(flow) = self.inline.as_mut() {
            flow.set_theme(theme);
        }
        if let Some(question) = self.question.as_mut() {
            question.set_theme(theme);
        }
    }

    pub(crate) fn desired_height(&mut self, width: u16) -> u16 {
        if let Some(flow) = self.inline.as_mut() {
            flow.desired_height(width).saturating_add(TOP_DIVIDER_ROWS)
        } else if let Some(approval) = self.approvals.front() {
            approval
                .desired_height(width)
                .saturating_add(TOP_DIVIDER_ROWS)
        } else if let Some(question) = self.question.as_mut() {
            // QuestionPanel already includes its own top divider row.
            question.desired_height(width).unwrap_or(8)
        } else {
            self.prompt.desired_height(width)
        }
    }

    pub(crate) fn render(&mut self, buf: &mut Buffer, area: Rect) {
        self.last_area = area;
        if let Some(flow) = self.inline.as_mut() {
            Self::render_with_top_divider(buf, area, self.divider_style, |buf, body| {
                flow.render(buf, body);
            });
        } else if let Some(approval) = self.approvals.front_mut() {
            Self::render_with_top_divider(buf, area, self.divider_style, |buf, body| {
                approval.render(buf, body);
            });
        } else if let Some(question) = self.question.as_mut() {
            // QuestionPanel renders its own top divider on row 0.
            question.render(buf, area);
        } else {
            self.prompt.render(buf, area);
        }
    }

    fn render_with_top_divider(
        buf: &mut Buffer,
        area: Rect,
        divider_style: Style,
        render_body: impl FnOnce(&mut Buffer, Rect),
    ) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        buf.set_string(
            area.x,
            area.y,
            "─".repeat(area.width as usize),
            divider_style,
        );
        let body = Rect::new(
            area.x,
            area.y.saturating_add(TOP_DIVIDER_ROWS),
            area.width,
            area.height.saturating_sub(TOP_DIVIDER_ROWS),
        );
        if body.height > 0 {
            render_body(buf, body);
        }
    }

    pub(crate) fn cursor_position(&self) -> Option<(u16, u16)> {
        if let Some(flow) = &self.inline {
            flow.cursor_position()
        } else if !self.approvals.is_empty() {
            None
        } else if let Some(question) = &self.question {
            question.cursor_position()
        } else {
            self.prompt.cursor_position()
        }
    }

    pub(crate) fn handle_paste(&mut self, text: &str) {
        if let Some(flow) = self.inline.as_mut() {
            flow.insert_paste(text);
        } else if !self.approvals.is_empty() {
            // Approval selection has no text field.
        } else if let Some(question) = self.question.as_mut() {
            question.handle_paste(text);
        } else {
            self.prompt.handle_paste(text);
        }
    }

    pub(crate) fn handle_key(
        &mut self,
        key: KeyEvent,
        is_toggle_mode: bool,
    ) -> (KeyResult, BottomAction) {
        if let Some(flow) = self.inline.as_mut() {
            return (KeyResult::Handled, BottomAction::Inline(flow.on_key(key)));
        }
        if let Some(approval) = self.approvals.front_mut() {
            return match approval.on_key(key) {
                ApprovalOutcome::Handled => (KeyResult::Handled, BottomAction::None),
                ApprovalOutcome::Decided { id, accepted } => {
                    self.approvals.pop_front();
                    (KeyResult::Handled, BottomAction::Approval { id, accepted })
                }
            };
        }
        if let Some(question) = self.question.as_mut() {
            let result = question.handle_key(key);
            if result.is_close() || question.is_finished() {
                self.question = None;
                return (KeyResult::Handled, BottomAction::QuestionClosed);
            }
            return (KeyResult::Handled, BottomAction::None);
        }
        let (result, action) = self.prompt.handle_key(key, is_toggle_mode);
        (result, BottomAction::Prompt(action))
    }

    pub(crate) fn handle_mouse(&mut self, event: MouseEvent) -> (MouseResult, BottomAction) {
        if let Some(flow) = self.inline.as_mut() {
            if rect_contains(flow.area(), event.column, event.row) {
                return (
                    MouseResult::Handled,
                    BottomAction::Inline(flow.on_mouse(event)),
                );
            }
            return (MouseResult::Ignored, BottomAction::None);
        }
        if let Some(approval) = self.approvals.front_mut() {
            if rect_contains(approval.area(), event.column, event.row) {
                return match approval.on_mouse(event) {
                    ApprovalOutcome::Handled => (MouseResult::Handled, BottomAction::None),
                    ApprovalOutcome::Decided { id, accepted } => {
                        self.approvals.pop_front();
                        (
                            MouseResult::Handled,
                            BottomAction::Approval { id, accepted },
                        )
                    }
                };
            }
            return (MouseResult::Ignored, BottomAction::None);
        }
        if let Some(panel) = self.question.as_mut() {
            if rect_contains(panel.area(), event.column, event.row) {
                panel.handle_mouse(event);
                if panel.is_finished() {
                    self.question = None;
                    return (MouseResult::Handled, BottomAction::QuestionClosed);
                }
                return (MouseResult::Handled, BottomAction::None);
            }
            return (MouseResult::Ignored, BottomAction::None);
        }
        (self.prompt.handle_mouse(event), BottomAction::None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solaris_agent::{ContextUsage, Mode};

    use crate::tui::connect::ConnectFlow;

    fn render_text(dock: &mut BottomDock, width: u16, height: u16) -> String {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        dock.render(&mut buf, area);
        (0..area.height)
            .map(|y| {
                let mut row = String::new();
                let mut x = 0u16;
                while x < area.width {
                    let sym = buf[(x, y)].symbol();
                    row.push_str(sym);
                    x += (solaris_tui::util::display_width(sym).max(1)) as u16;
                }
                row
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn peer_inline_surface_replaces_prompt_and_status_bar_while_preserving_draft() {
        let theme = Theme::dark();
        let mut editor = Editor::with_placeholder("Ask anything…");
        editor.set_text("unfinished draft");
        let mut dock = BottomDock::new(editor, &theme);
        dock.sync_prompt(
            StatusBarState {
                label: "fake".into(),
                model: "m1".into(),
                mode: Mode::Default,
                context: ContextUsage {
                    tokens: Some(0),
                    window: 1000,
                    estimated: false,
                },
            },
            true,
        );

        assert_eq!(dock.active_surface(), BottomSurface::Prompt);
        let prompt_frame = render_text(&mut dock, 80, 5);
        assert!(
            !prompt_frame
                .lines()
                .next()
                .unwrap_or_default()
                .starts_with("────"),
            "PromptContainer should not render a top horizontal divider:\n{prompt_frame}"
        );
        assert!(prompt_frame.contains("unfinished draft"), "{prompt_frame}");
        assert!(prompt_frame.contains("DEFAULT"), "{prompt_frame}");
        assert!(prompt_frame.contains("上下文"), "{prompt_frame}");

        // Switching to a peer Inline surface hides both the editor and status bar
        // and renders a full-width top divider line.
        dock.set_inline(Some(Inline::Connect(ConnectFlow::new(&theme))));
        assert_eq!(dock.active_surface(), BottomSurface::Inline);
        let inline_frame = render_text(&mut dock, 80, 12);
        assert_eq!(
            inline_frame.lines().next().unwrap_or_default(),
            "─".repeat(80),
            "Inline surface should start with a full-width top divider:\n{inline_frame}"
        );
        assert!(inline_frame.contains("Connect"), "{inline_frame}");
        assert!(
            !inline_frame.contains("unfinished draft") && !inline_frame.contains("上下文"),
            "peer surface must replace the entire PromptContainer:\n{inline_frame}"
        );

        // Approval surface also renders a full-width top divider line.
        dock.clear_inline();
        dock.push_approval(ApprovalPanel::new(
            1,
            "bash",
            "Run cargo test".to_string(),
            &theme,
        ));
        assert_eq!(dock.active_surface(), BottomSurface::Approval);
        let approval_frame = render_text(&mut dock, 80, 10);
        assert_eq!(
            approval_frame.lines().next().unwrap_or_default(),
            "─".repeat(80),
            "Approval surface should start with a full-width top divider:\n{approval_frame}"
        );
        assert!(approval_frame.contains("bash"), "{approval_frame}");

        // Closing the peer restores PromptContainer with the original draft intact.
        dock.remove_approval(1);
        assert_eq!(dock.active_surface(), BottomSurface::Prompt);
        let restored = render_text(&mut dock, 80, 5);
        assert!(restored.contains("unfinished draft"), "{restored}");
        assert!(restored.contains("DEFAULT"), "{restored}");
    }
}
