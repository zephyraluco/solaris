//! A scrolling viewport over a taller child component.
//!
//! The child is rendered once into an offscreen buffer sized to its natural
//! height, then the visible window is blitted into the frame. `desired_height`
//! and `version` on [`Component`] are what make this cheap: the canvas is only
//! rebuilt when the width, content height, or content revision changes.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::component::{Component, KeyResult, MouseResult};
use crate::util::rect_contains;

/// Upper bound on the offscreen canvas height, to bound memory.
const MAX_CANVAS_ROWS: u32 = 20_000;

struct Cache {
    width: u16,
    height: u16,
    version: u64,
    buffer: Buffer,
}

/// A vertically scrollable window onto `content`.
pub struct ScrollView {
    content: Box<dyn Component>,
    follow_end: bool,
    offset: u16,
    max_offset: u16,
    last_area: Rect,
    cache: Option<Cache>,
}

impl ScrollView {
    /// Wrap `content` in a scroll view that follows the end by default.
    pub fn new(content: Box<dyn Component>) -> Self {
        Self {
            content,
            follow_end: true,
            offset: 0,
            max_offset: 0,
            last_area: Rect::default(),
            cache: None,
        }
    }

    /// Whether new content auto-scrolls into view.
    pub fn follow_end(mut self, follow: bool) -> Self {
        self.follow_end = follow;
        self
    }

    /// Current scroll offset in rows from the top.
    pub fn offset(&self) -> u16 {
        self.offset
    }

    /// Largest valid offset.
    pub fn max_offset(&self) -> u16 {
        self.max_offset
    }

    /// Whether the view is pinned to the bottom.
    pub fn is_at_end(&self) -> bool {
        self.offset >= self.max_offset
    }

    /// Rows currently visible.
    pub fn viewport_height(&self) -> u16 {
        self.last_area.height
    }

    /// Scroll by `lines` (negative scrolls up).
    pub fn scroll_by(&mut self, lines: i32) {
        let target = (self.offset as i32 + lines).clamp(0, self.max_offset as i32);
        self.offset = target as u16;
        self.follow_end = self.offset >= self.max_offset;
    }

    /// Jump to the bottom and resume following.
    pub fn scroll_to_end(&mut self) {
        self.offset = self.max_offset;
        self.follow_end = true;
    }

    /// Jump to the top and stop following.
    pub fn scroll_to_start(&mut self) {
        self.offset = 0;
        self.follow_end = false;
    }

    /// Mutable access to the wrapped content.
    pub fn content_mut(&mut self) -> &mut (dyn Component + 'static) {
        self.content.as_mut()
    }
}

impl Component for ScrollView {
    fn render(&mut self, buf: &mut Buffer, area: Rect) {
        self.last_area = area;
        if area.width == 0 || area.height == 0 {
            return;
        }

        let version = self.content.version();
        let natural = self
            .content
            .desired_height(area.width)
            .unwrap_or(area.height);
        let canvas_height = natural.max(area.height);
        self.max_offset = canvas_height.saturating_sub(area.height);

        if self.follow_end {
            self.offset = self.max_offset;
        }
        self.offset = self.offset.min(self.max_offset);

        let needs_rebuild = match &self.cache {
            Some(cache) => {
                cache.width != area.width
                    || cache.height != canvas_height
                    || (version != 0 && cache.version != version)
            }
            None => true,
        };

        if needs_rebuild {
            let canvas_area = Rect::new(0, 0, area.width, canvas_height);
            let mut canvas = Buffer::empty(canvas_area);
            self.content.render(&mut canvas, canvas_area);
            self.cache = Some(Cache {
                width: area.width,
                height: canvas_height,
                version,
                buffer: canvas,
            });
        }

        let Some(cache) = self.cache.as_ref() else {
            return;
        };

        let rows = area.height.min(canvas_height);
        for row in 0..rows {
            let source_y = self.offset.saturating_add(row);
            if source_y >= cache.height {
                break;
            }
            for column in 0..area.width {
                buf[(area.x + column, area.y + row)] = cache.buffer[(column, source_y)].clone();
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> KeyResult {
        if key.modifiers.difference(KeyModifiers::SHIFT) != KeyModifiers::empty() {
            return KeyResult::Ignored;
        }
        let page = self.last_area.height.max(1) as i32;
        match key.code {
            KeyCode::PageUp => {
                self.scroll_by(-page);
                KeyResult::Handled
            }
            KeyCode::PageDown => {
                self.scroll_by(page);
                KeyResult::Handled
            }
            KeyCode::Home => {
                self.scroll_to_start();
                KeyResult::Handled
            }
            KeyCode::End => {
                self.scroll_to_end();
                KeyResult::Handled
            }
            KeyCode::Up => {
                self.scroll_by(-1);
                KeyResult::Handled
            }
            KeyCode::Down => {
                self.scroll_by(1);
                KeyResult::Handled
            }
            _ => KeyResult::Ignored,
        }
    }

    fn handle_mouse(&mut self, event: MouseEvent) -> MouseResult {
        if !rect_contains(self.last_area, event.column, event.row) {
            return MouseResult::Ignored;
        }
        match event.kind {
            MouseEventKind::ScrollUp => {
                self.scroll_by(-3);
                MouseResult::Handled
            }
            MouseEventKind::ScrollDown => {
                self.scroll_by(3);
                MouseResult::Handled
            }
            _ => MouseResult::Ignored,
        }
    }

    fn tick(&mut self) -> bool {
        self.content.tick()
    }

    fn invalidate(&mut self) {
        self.cache = None;
        self.content.invalidate();
    }

    fn desired_height(&mut self, width: u16) -> Option<u16> {
        self.content
            .desired_height(width)
            .map(|height| height.min(MAX_CANVAS_ROWS as u16))
    }

    fn version(&self) -> u64 {
        self.content.version()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::Text;

    fn buffer(width: u16, height: u16) -> Buffer {
        Buffer::empty(Rect::new(0, 0, width, height))
    }

    #[test]
    fn follows_end_while_streaming() {
        let mut view = ScrollView::new(Box::new(Text::new("1\n2\n3\n4\n5\n6")));
        let area = Rect::new(0, 0, 10, 3);
        let mut buf = buffer(10, 3);
        view.render(&mut buf, area);

        assert_eq!(view.max_offset(), 3);
        assert_eq!(view.offset(), 3);
        // Bottom three lines are visible.
        assert_eq!(buf[(0, 0)].symbol(), "4");
        assert_eq!(buf[(0, 2)].symbol(), "6");
    }

    #[test]
    fn scrolling_up_stops_following() {
        let mut view = ScrollView::new(Box::new(Text::new("1\n2\n3\n4\n5\n6")));
        let area = Rect::new(0, 0, 10, 3);
        let mut buf = buffer(10, 3);
        view.render(&mut buf, area);

        view.scroll_by(-2);
        assert!(!view.is_at_end());
        assert_eq!(view.offset(), 1);
        view.render(&mut buf, area);
        assert_eq!(buf[(0, 0)].symbol(), "2");
    }

    #[test]
    fn short_content_does_not_scroll() {
        let mut view = ScrollView::new(Box::new(Text::new("only")));
        let area = Rect::new(0, 0, 10, 5);
        let mut buf = buffer(10, 5);
        view.render(&mut buf, area);
        assert_eq!(view.max_offset(), 0);
        assert!(view.is_at_end());
    }

    #[test]
    fn mouse_wheel_is_ignored_outside_the_viewport() {
        let mut view = ScrollView::new(Box::new(Text::new("1\n2\n3\n4\n5\n6")));
        let area = Rect::new(0, 0, 10, 2);
        let mut buf = buffer(10, 2);
        view.render(&mut buf, area);

        let outside = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 50,
            row: 50,
            modifiers: KeyModifiers::empty(),
        };
        assert_eq!(view.handle_mouse(outside), MouseResult::Ignored);
    }
}
