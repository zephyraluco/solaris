//! Top-level session page container: vertical split of [`TranscriptContainer`]
//! (top) and [`BottomDock`] (bottom).

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use solaris_tui::components::editor::Editor;
use solaris_tui::components::welcome::WelcomeData;
use solaris_tui::layout::{self, Axis, Entry};
use solaris_tui::selection::SelectionHandle;
use solaris_tui::theme::Theme;

use super::bottom_dock::BottomDock;
use super::transcript_pane::TranscriptContainer;
use crate::tui::state::SessionState;

/// Main session screen composed from two containers: transcript + bottom dock.
pub(crate) struct SessionContainer {
    pub(crate) transcript: TranscriptContainer,
    pub(crate) bottom: BottomDock,
}

impl SessionContainer {
    pub(crate) fn new(editor: Editor, theme: &Theme) -> Self {
        Self {
            transcript: TranscriptContainer::new(),
            bottom: BottomDock::new(editor, theme),
        }
    }

    pub(crate) fn set_theme(&mut self, theme: &Theme) {
        self.transcript.invalidate();
        self.bottom.set_theme(theme);
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render(
        &mut self,
        buf: &mut Buffer,
        area: Rect,
        session: &mut SessionState,
        theme: &Theme,
        spinner: Option<u16>,
        welcome: &WelcomeData,
        selection: &SelectionHandle,
    ) {
        if area.width < 12 || area.height < 4 {
            self.transcript.set_area(Rect::default());
            self.transcript.cancel_interactions();
            return;
        }

        let max_bottom = area.height.saturating_sub(1).max(3);
        let bottom_height = self.bottom.desired_height(area.width).clamp(3, max_bottom);
        let rows = layout::split(
            area,
            Axis::Vertical,
            &[Entry::auto().grow(1).min(1), Entry::px(bottom_height)],
        );

        self.transcript
            .render(buf, rows[0], session, theme, spinner, welcome, selection);
        self.bottom.render(buf, rows[1]);
    }
}
