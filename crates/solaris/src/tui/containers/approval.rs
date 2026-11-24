//! Inline tool-approval panel styled like `InlineFlow` (`/connect`, `/model`).
//!
//! Renders as a peer inside [`BottomDock`](super::bottom_dock::BottomDock)
//! instead of opening a modal overlay dialog.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use solaris_tui::components::inline_select::{InlineSelect, InlineSelectOutcome, InlineStyles};
use solaris_tui::components::select_list::SelectItem;
use solaris_tui::theme::Theme;

/// Result of handling a key or mouse event in [`ApprovalPanel`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApprovalOutcome {
    /// Event consumed; the panel stays active.
    Handled,
    /// User chose whether the tool call may run.
    Decided { id: u64, accepted: bool },
}

/// Inline yes/no confirmation surface for a pending tool call.
pub(crate) struct ApprovalPanel {
    id: u64,
    picker: InlineSelect,
}

impl ApprovalPanel {
    pub(crate) fn new(id: u64, tool_name: &str, details: String, theme: &Theme) -> Self {
        let items = vec![
            SelectItem::new("yes", "Yes").description("Allow this call to run"),
            SelectItem::new("no", "No").description("Decline and skip this call"),
        ];
        let mut picker = InlineSelect::new(
            InlineStyles::from_theme(theme),
            format!("Run {tool_name}?"),
            "Allow this tool call?",
            items,
        );
        if !details.trim().is_empty() {
            picker = picker.with_note(details);
        }
        // Default highlight on No so an accidental Enter never approves a tool.
        picker.select_value("no");
        Self { id, picker }
    }

    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    pub(crate) fn set_theme(&mut self, theme: &Theme) {
        self.picker.set_styles(InlineStyles::from_theme(theme));
    }

    pub(crate) fn area(&self) -> Rect {
        self.picker.area()
    }

    pub(crate) fn desired_height(&self, width: u16) -> u16 {
        self.picker.desired_height(width)
    }

    pub(crate) fn render(&mut self, buf: &mut Buffer, area: Rect) {
        self.picker.render(buf, area);
    }

    pub(crate) fn on_key(&mut self, key: KeyEvent) -> ApprovalOutcome {
        if key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            return ApprovalOutcome::Decided {
                id: self.id,
                accepted: false,
            };
        }
        if key.modifiers.is_empty() {
            match key.code {
                KeyCode::Char('y' | 'Y') => {
                    return ApprovalOutcome::Decided {
                        id: self.id,
                        accepted: true,
                    };
                }
                KeyCode::Char('n' | 'N') => {
                    return ApprovalOutcome::Decided {
                        id: self.id,
                        accepted: false,
                    };
                }
                _ => {}
            }
        }
        match self.picker.on_key(key) {
            InlineSelectOutcome::Picked(_) => self.picked(),
            InlineSelectOutcome::Handled => ApprovalOutcome::Handled,
        }
    }

    pub(crate) fn on_mouse(&mut self, mouse: MouseEvent) -> ApprovalOutcome {
        match self.picker.on_mouse(mouse) {
            InlineSelectOutcome::Picked(_) => self.picked(),
            InlineSelectOutcome::Handled => ApprovalOutcome::Handled,
        }
    }

    fn picked(&self) -> ApprovalOutcome {
        let accepted = self.picker.selected_value() == Some("yes");
        ApprovalOutcome::Decided {
            id: self.id,
            accepted,
        }
    }
}
