//! Main transcript viewport container: scrolling, selection registration, and
//! collapsible / subagent click interaction.

use crossterm::event::MouseEvent;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use solaris_tui::components::collapsible::CollapsibleInteraction;
use solaris_tui::components::welcome::WelcomeData;
use solaris_tui::selection::SelectionHandle;
use solaris_tui::theme::Theme;

use crate::tui::state::SessionState;
use crate::tui::view::transcript::{CollapsibleHeader, TranscriptView};

/// Container owning the main transcript viewport and its mouse interaction state.
pub(crate) struct TranscriptContainer {
    view: TranscriptView,
    last_area: Rect,
    collapsible_mouse: CollapsibleInteraction<(usize, usize, bool)>,
}

impl TranscriptContainer {
    pub(crate) fn new() -> Self {
        Self {
            view: TranscriptView::new(),
            last_area: Rect::default(),
            collapsible_mouse: CollapsibleInteraction::default(),
        }
    }

    pub(crate) fn area(&self) -> Rect {
        self.last_area
    }

    pub(crate) fn set_area(&mut self, area: Rect) {
        self.last_area = area;
    }

    pub(crate) fn invalidate(&mut self) {
        self.view.invalidate();
    }

    pub(crate) fn cancel_interactions(&mut self) {
        self.collapsible_mouse.cancel();
        self.view.cancel_subagent_interaction();
    }

    pub(crate) fn cancel_collapsible(&mut self) {
        self.collapsible_mouse.cancel();
    }

    pub(crate) fn collapsible_at(&self, row: usize) -> Option<CollapsibleHeader> {
        self.view.collapsible_at(row)
    }

    pub(crate) fn collapsible_mouse_mut(
        &mut self,
    ) -> &mut CollapsibleInteraction<(usize, usize, bool)> {
        &mut self.collapsible_mouse
    }

    pub(crate) fn handle_subagent_mouse(
        &mut self,
        event: MouseEvent,
        scroll: usize,
        selected: bool,
    ) -> (bool, Option<(usize, usize)>) {
        self.view
            .handle_subagent_mouse(event, self.last_area, scroll, selected)
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
        self.last_area = area;
        if area.width == 0 || area.height == 0 {
            return;
        }

        let lines = self.view.lines(session, area, theme, spinner, welcome);

        let visible = area.height as usize;
        let max_scroll = lines.len().saturating_sub(visible);

        if session.follow_end {
            session.scroll = max_scroll;
        }
        session.scroll = session.scroll.min(max_scroll);
        // Re-arm following once the view is back at the bottom.
        if session.scroll >= max_scroll {
            session.follow_end = true;
        }

        for (row, line) in lines.iter().skip(session.scroll).take(visible).enumerate() {
            buf.set_line(area.x, area.y + row as u16, line, area.width);
        }
        selection
            .borrow_mut()
            .set_document(area, session.scroll, self.view.document());
    }
}
